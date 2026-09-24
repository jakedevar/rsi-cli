//! Daemon registry adapter. Projects and verified sandbox custody come from
//! the main Store; Git's exact worktree list admits contained detached roots.
//! Session and agent supplied paths never register workspaces.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use rsi_common::types::Project;
use tokio::task::JoinHandle;
use tracing::warn;
use uuid::Uuid;

use super::{IndexHandle, IndexManager, IndexWatcher, RegisteredWorkspace, Result};
use crate::bus::EventBus;
use crate::sandbox::git_worktree::discover_registered_worktree_roots;
use crate::store::Store;

type SandboxRegistration = (Uuid, PathBuf, PathBuf);
const REGISTRY_REFRESH: Duration = Duration::from_secs(5);
// The manager accepts at most 128 registered workspaces in one snapshot.
const MAX_DETACHED_DISCOVERY_ENTRIES: usize = 128;

pub struct IndexRuntime {
    handle: IndexHandle,
    watchers: HashMap<Uuid, (PathBuf, IndexWatcher)>,
    task: JoinHandle<()>,
    registry_task: Option<JoinHandle<()>>,
}

impl IndexRuntime {
    pub fn start(
        index_root: PathBuf,
        projects: Vec<Project>,
        allowed_roots: &[PathBuf],
    ) -> Result<Self> {
        Self::start_with_registrations(index_root, projects, Vec::new(), allowed_roots)
    }

    pub fn start_with_registrations(
        index_root: PathBuf,
        projects: Vec<Project>,
        sandboxes: Vec<SandboxRegistration>,
        allowed_roots: &[PathBuf],
    ) -> Result<Self> {
        Self::start_with_registrations_and_bus(index_root, projects, sandboxes, allowed_roots, None)
    }

    pub fn start_with_registrations_and_bus(
        index_root: PathBuf,
        projects: Vec<Project>,
        sandboxes: Vec<SandboxRegistration>,
        allowed_roots: &[PathBuf],
        events: Option<Arc<EventBus>>,
    ) -> Result<Self> {
        Self::start_with_registrations_and_bus_and_gate(
            index_root,
            projects,
            sandboxes,
            allowed_roots,
            events,
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// Share the persisted operator setting with the manager's live admission
    /// gate. Direct atomic changes are observed by the manager's rescan tick.
    pub fn start_with_registrations_and_bus_and_gate(
        index_root: PathBuf,
        projects: Vec<Project>,
        sandboxes: Vec<SandboxRegistration>,
        allowed_roots: &[PathBuf],
        events: Option<Arc<EventBus>>,
        enabled: Arc<AtomicBool>,
    ) -> Result<Self> {
        let workspaces = registered_workspaces(projects, sandboxes, allowed_roots);
        let (manager, handle) =
            IndexManager::new_with_enabled(index_root, workspaces.clone(), enabled)?;
        let manager = match events {
            Some(events) => manager.with_event_bus(events),
            None => manager,
        };
        let watchers = watch(&workspaces, &handle, HashMap::new());
        let task = tokio::spawn(manager.run());
        Ok(Self {
            handle,
            watchers,
            task,
            registry_task: None,
        })
    }

    /// Refresh registrations from the same persisted Store as project CRUD and
    /// custody settlement. Changes are admitted without restarting the writer.
    pub fn attach_registry(
        &mut self,
        store: Arc<tokio::sync::Mutex<Store>>,
        allowed_roots: Vec<PathBuf>,
    ) {
        let handle = self.handle.clone();
        let initial_watchers = std::mem::take(&mut self.watchers);
        self.registry_task = Some(tokio::spawn(async move {
            let mut watchers = initial_watchers;
            let mut tick = tokio::time::interval(REGISTRY_REFRESH);
            loop {
                tick.tick().await;
                let source = Arc::clone(&store);
                let roots = allowed_roots.clone();
                let snapshot = tokio::task::spawn_blocking(move || {
                    let store = source.blocking_lock();
                    let projects = store.load_projects()?;
                    let sandboxes = store.list_codegraph_sandbox_registrations()?;
                    drop(store);
                    Ok::<_, crate::error::DaemonError>(registered_workspaces_with_detached(
                        projects, sandboxes, &roots,
                    ))
                })
                .await;
                let workspaces = match snapshot {
                    Ok(Ok(snapshot)) => snapshot,
                    Ok(Err(error)) => {
                        warn!(%error, "Codegraph registry refresh failed");
                        continue;
                    }
                    Err(error) => {
                        warn!(%error, "Codegraph registry worker stopped");
                        continue;
                    }
                };
                if let Err(error) = handle.reconcile(workspaces.clone()) {
                    warn!(%error, "Codegraph registry snapshot rejected");
                    continue;
                }
                watchers = watch(&workspaces, &handle, watchers);
            }
        }));
    }

    pub fn handle(&self) -> &IndexHandle {
        &self.handle
    }
}

fn registered_workspaces(
    projects: Vec<Project>,
    sandboxes: Vec<SandboxRegistration>,
    allowed_roots: &[PathBuf],
) -> Vec<RegisteredWorkspace> {
    let mut workspaces = Vec::new();
    let mut project_roots: Vec<(PathBuf, Uuid)> = Vec::new();
    for project in projects {
        let Some(path) = project.path else {
            continue;
        };
        match RegisteredWorkspace::primary(project.id, &path) {
            Ok(workspace) if allowed(&workspace, allowed_roots) => {
                project_roots.push((workspace.root().to_path_buf(), project.id));
                workspaces.push(workspace);
            }
            Ok(_) => {
                warn!(project_id = %project.id, "Codegraph project root is outside configured workspace roots")
            }
            Err(error) => {
                warn!(project_id = %project.id, %error, "Codegraph project root unavailable")
            }
        }
    }
    for (custody_id, repo, root) in sandboxes {
        let Ok(repo) = repo.canonicalize() else {
            continue;
        };
        // The registered project may contain the source checkout rather than
        // equal it. Use the same longest-prefix rule as project resolution.
        let longest = project_roots
            .iter()
            .filter(|(path, _)| repo.starts_with(path))
            .map(|(path, _)| path.components().count())
            .max();
        let Some(longest) = longest else {
            continue;
        };
        let matches = project_roots
            .iter()
            .filter(|(path, _)| repo.starts_with(path) && path.components().count() == longest)
            .collect::<Vec<_>>();
        // Identical registered paths under distinct project IDs are ambiguous.
        let [(_, project_id)] = matches.as_slice() else {
            continue;
        };
        match RegisteredWorkspace::registered_checkout(
            *project_id,
            &repo,
            &root,
            rsi_codegraph::WorkspaceInstanceKey::RsiSandbox(custody_id),
        ) {
            Ok(workspace) => workspaces.push(workspace),
            Err(error) => warn!(%custody_id, %error, "Codegraph sandbox unavailable"),
        }
    }
    workspaces
}

/// Detached admission comes only from Git's registered worktree list. Keep
/// this entire read on the blocking registry worker, never the async tick.
fn registered_workspaces_with_detached(
    projects: Vec<Project>,
    sandboxes: Vec<SandboxRegistration>,
    allowed_roots: &[PathBuf],
) -> Vec<RegisteredWorkspace> {
    let mut workspaces = registered_workspaces(projects, sandboxes, allowed_roots);
    if allowed_roots.is_empty() {
        return workspaces;
    }

    let registered_roots: HashSet<PathBuf> = workspaces
        .iter()
        .map(|workspace| workspace.root().to_path_buf())
        .collect();
    let mut candidates: HashMap<PathBuf, Vec<(Uuid, PathBuf)>> = HashMap::new();
    for primary in workspaces.iter().filter(|workspace| {
        rsi_codegraph::CodegraphStore::workspace_id(workspace.project_id(), "primary")
            .is_ok_and(|id| id == workspace.workspace_id())
    }) {
        if !primary.root().join(".git").exists() {
            continue;
        }
        match discover_registered_worktree_roots(primary.root(), MAX_DETACHED_DISCOVERY_ENTRIES) {
            Ok(roots) => {
                for root in roots {
                    if !registered_roots.contains(&root)
                        && allowed_roots.iter().any(|allowed| contains(allowed, &root))
                    {
                        candidates
                            .entry(root)
                            .or_default()
                            .push((primary.project_id(), primary.root().to_path_buf()));
                    }
                }
            }
            Err(error) => {
                warn!(project_id = %primary.project_id(), %error, "Codegraph detached discovery unavailable")
            }
        }
    }
    if workspaces.len().saturating_add(candidates.len()) > MAX_DETACHED_DISCOVERY_ENTRIES {
        warn!("Codegraph detached registrations exceed workspace snapshot bound");
        return workspaces;
    }
    for (root, owners) in candidates {
        let [(project_id, project_root)] = owners.as_slice() else {
            warn!(path = %root.display(), "Codegraph detached root has ambiguous project ownership");
            continue;
        };
        match RegisteredWorkspace::detached_checkout(*project_id, project_root, &root) {
            Ok(workspace)
                if workspace.root() == root
                    && allowed_roots
                        .iter()
                        .any(|allowed| contains(allowed, workspace.root())) =>
            {
                workspaces.push(workspace);
            }
            Ok(_) => {
                warn!(path = %root.display(), "Codegraph detached root changed during admission")
            }
            Err(error) => {
                warn!(path = %root.display(), %error, "Codegraph detached checkout unavailable")
            }
        }
    }
    workspaces
}

fn watch(
    workspaces: &[RegisteredWorkspace],
    handle: &IndexHandle,
    mut previous: HashMap<Uuid, (PathBuf, IndexWatcher)>,
) -> HashMap<Uuid, (PathBuf, IndexWatcher)> {
    let mut next = HashMap::new();
    for workspace in workspaces {
        let id = workspace.workspace_id();
        if let Some((root, watcher)) = previous.remove(&id)
            && root == workspace.root()
        {
            next.insert(id, (root, watcher));
            continue;
        }
        match IndexWatcher::new(workspace, handle.clone()) {
            Ok(watcher) => {
                next.insert(id, (workspace.root().to_path_buf(), watcher));
            }
            Err(error) => {
                warn!(project_id = %workspace.project_id(), workspace_id = %id, %error, "Codegraph watcher unavailable; periodic reconciliation remains active")
            }
        }
    }
    next
}

impl Drop for IndexRuntime {
    fn drop(&mut self) {
        if let Some(task) = &self.registry_task {
            task.abort();
        }
        self.task.abort();
    }
}

fn allowed(workspace: &RegisteredWorkspace, allowed_roots: &[PathBuf]) -> bool {
    allowed_roots.is_empty()
        || allowed_roots
            .iter()
            .any(|root| contains(root, workspace.root()))
}

fn contains(root: &Path, candidate: &Path) -> bool {
    root.canonicalize()
        .is_ok_and(|root| candidate.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RuntimeConfig};
    use chrono::Utc;
    use std::process::Command;
    use std::sync::atomic::Ordering;
    use uuid::Uuid;

    fn project(id: Uuid, path: PathBuf) -> Project {
        Project {
            id,
            name: "registered".into(),
            path: Some(path),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn git_fixture() -> (tempfile::TempDir, PathBuf) {
        let base = tempfile::tempdir().unwrap();
        let repo = base.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("lib.rs"), "pub fn base() {}\n").unwrap();
        git(&repo, &["add", "lib.rs"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Codegraph Test",
                "-c",
                "user.email=codegraph@example.invalid",
                "commit",
                "-qm",
                "base",
            ],
        );
        (base, repo)
    }

    fn add_worktree(repo: &Path, root: &Path) {
        git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                "-q",
                root.to_str().unwrap(),
                "HEAD",
            ],
        );
    }

    #[test]
    fn detached_registration_requires_discovery_containment_and_stable_identity() {
        let (base, repo) = git_fixture();
        let inside = base.path().join("inside");
        let outside = tempfile::tempdir().unwrap();
        let outside_root = outside.path().join("checkout");
        add_worktree(&repo, &inside);
        add_worktree(&repo, &outside_root);
        let id = Uuid::new_v4();
        let registered = vec![project(id, repo.clone())];
        let roots = vec![base.path().to_path_buf()];

        assert_eq!(
            registered_workspaces_with_detached(registered.clone(), Vec::new(), &[]).len(),
            1
        );
        let first = registered_workspaces_with_detached(registered.clone(), Vec::new(), &roots);
        let second = registered_workspaces_with_detached(registered, Vec::new(), &roots);
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        let detached = RegisteredWorkspace::detached_checkout(id, &repo, &inside).unwrap();
        assert_eq!(first[1].workspace_id(), detached.workspace_id());
        assert_eq!(second[1].workspace_id(), detached.workspace_id());

        let custody = Uuid::new_v4();
        let with_custody = registered_workspaces_with_detached(
            vec![project(id, repo.clone())],
            vec![(custody, repo, inside)],
            &roots,
        );
        assert_eq!(with_custody.len(), 2);
        assert_ne!(with_custody[1].workspace_id(), detached.workspace_id());
    }

    #[test]
    fn detached_registration_rejects_ambiguous_stale_and_symlink_escape_roots() {
        let (base, repo) = git_fixture();
        let second_project = base.path().join("second-project");
        let candidate = base.path().join("candidate");
        add_worktree(&repo, &second_project);
        add_worktree(&repo, &candidate);
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let roots = vec![base.path().to_path_buf()];
        let projects = vec![
            project(first, repo.clone()),
            project(second, second_project),
        ];
        let ambiguous = registered_workspaces_with_detached(projects, Vec::new(), &roots);
        assert_eq!(ambiguous.len(), 2);

        let outside = tempfile::tempdir().unwrap();
        let alias = base.path().join("alias");
        std::os::unix::fs::symlink(outside.path(), &alias).unwrap();
        add_worktree(&repo, &alias.join("escaped"));
        let excluded = registered_workspaces_with_detached(
            vec![project(first, repo.clone())],
            Vec::new(),
            &roots,
        );
        assert_eq!(excluded.len(), 3);
        assert!(
            excluded
                .iter()
                .all(|workspace| workspace.root() != outside.path().join("escaped"))
        );

        std::fs::remove_dir_all(&candidate).unwrap();
        let remaining =
            registered_workspaces_with_detached(vec![project(first, repo)], Vec::new(), &roots);
        assert_eq!(remaining.len(), 2);
        assert!(
            remaining
                .iter()
                .all(|workspace| workspace.root() != candidate)
        );

        let foreign = tempfile::tempdir().unwrap();
        git(foreign.path(), &["init", "-q"]);
        std::fs::write(
            base.path().join("second-project/.git"),
            format!("gitdir: {}\n", foreign.path().join(".git").display()),
        )
        .unwrap();
        let wrong_repository = registered_workspaces_with_detached(
            vec![project(first, base.path().join("repo"))],
            Vec::new(),
            &roots,
        );
        assert_eq!(wrong_repository.len(), 1);
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    #[tokio::test]
    async fn same_head_dirty_custody_worktrees_keep_distinct_bound_heads() {
        let roots = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        let repo = roots.path().join("repo");
        let first_root = roots.path().join("first");
        let second_root = roots.path().join("second");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("lib.rs"), "pub fn base() {}\n").unwrap();
        git(&repo, &["add", "lib.rs"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Codegraph Test",
                "-c",
                "user.email=codegraph@example.invalid",
                "commit",
                "-qm",
                "base",
            ],
        );
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                "-q",
                first_root.to_str().unwrap(),
                "HEAD",
            ],
        );
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                "-q",
                second_root.to_str().unwrap(),
                "HEAD",
            ],
        );
        assert_eq!(
            git(&first_root, &["rev-parse", "HEAD"]),
            git(&second_root, &["rev-parse", "HEAD"])
        );
        std::fs::write(first_root.join("lib.rs"), "pub fn dirty_first() {}\n").unwrap();
        std::fs::write(second_root.join("lib.rs"), "pub fn dirty_second() {}\n").unwrap();
        assert!(git(&first_root, &["status", "--porcelain"]).contains("lib.rs"));
        assert!(git(&second_root, &["status", "--porcelain"]).contains("lib.rs"));

        let project_id = Uuid::new_v4();
        let first_custody = Uuid::new_v4();
        let second_custody = Uuid::new_v4();
        let project = Project {
            id: project_id,
            name: "dirty worktrees".into(),
            path: Some(repo.clone()),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let first = RegisteredWorkspace::registered_checkout(
            project_id,
            &repo,
            &first_root,
            rsi_codegraph::WorkspaceInstanceKey::RsiSandbox(first_custody),
        )
        .unwrap();
        let second = RegisteredWorkspace::registered_checkout(
            project_id,
            &repo,
            &second_root,
            rsi_codegraph::WorkspaceInstanceKey::RsiSandbox(second_custody),
        )
        .unwrap();
        let detached_first =
            RegisteredWorkspace::detached_checkout(project_id, &repo, &first_root).unwrap();
        let detached_second =
            RegisteredWorkspace::detached_checkout(project_id, &repo, &second_root).unwrap();
        assert_ne!(
            detached_first.workspace_id(),
            detached_second.workspace_id()
        );
        assert_ne!(detached_first.workspace_id(), first.workspace_id());
        let runtime = IndexRuntime::start_with_registrations_and_bus_and_gate(
            indexes.path().to_path_buf(),
            vec![project],
            vec![
                (first_custody, repo.clone(), first_root.clone()),
                (second_custody, repo.clone(), second_root.clone()),
            ],
            &[],
            None,
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap();
        let handle = runtime.handle();
        let bound = handle.registered_project_workspaces(project_id).unwrap();
        assert_eq!(bound.len(), 3);
        let first_binding = handle
            .registered_workspace(first.workspace_id())
            .unwrap()
            .unwrap();
        let second_binding = handle
            .registered_workspace(second.workspace_id())
            .unwrap()
            .unwrap();
        assert_eq!(
            first_binding.workspace.root(),
            first_root.canonicalize().unwrap()
        );
        assert_eq!(
            second_binding.workspace.root(),
            second_root.canonicalize().unwrap()
        );
        assert_eq!(first_binding.db_path, second_binding.db_path);
        assert!(
            handle
                .registered_workspace(Uuid::new_v4())
                .unwrap()
                .is_none()
        );

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let first_ready = handle
                    .status(first.workspace_id())
                    .is_some_and(|s| s.phase == super::super::IndexPhase::Ready);
                let second_ready = handle
                    .status(second.workspace_id())
                    .is_some_and(|s| s.phase == super::super::IndexPhase::Ready);
                if first_ready && second_ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let db = first_binding.db_path;
        let store = rsi_codegraph::CodegraphStore::open(&db, project_id).unwrap();
        let first_ready = store.current_ready(first.workspace_id()).unwrap();
        let second_ready = store.current_ready(second.workspace_id()).unwrap();
        assert_ne!(first_ready.snapshot_digest, second_ready.snapshot_digest);

        std::fs::write(first_root.join("lib.rs"), "pub fn dirty_first_again() {}\n").unwrap();
        handle.request(first.workspace_id()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let current = rsi_codegraph::CodegraphStore::open(&db, project_id)
                    .unwrap()
                    .current_ready(first.workspace_id())
                    .unwrap();
                if current != first_ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let store = rsi_codegraph::CodegraphStore::open(&db, project_id).unwrap();
        assert_eq!(
            store.current_ready(second.workspace_id()).unwrap(),
            second_ready
        );
        handle.reconcile(Vec::new()).unwrap();
        assert!(
            handle
                .registered_workspace(first.workspace_id())
                .unwrap()
                .is_none()
        );
        assert!(
            handle
                .registered_project_workspaces(project_id)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn persisted_shared_gate_reconciles_live_and_retains_ready_across_restart() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        let settings_path = indexes.path().join("daemon.sqlite");
        let settings_store = Store::open(&settings_path).unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn first() {}\n").unwrap();
        let project = Project {
            id: Uuid::new_v4(),
            name: "gate".into(),
            path: Some(root.path().to_path_buf()),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let workspace = RegisteredWorkspace::primary(project.id, root.path()).unwrap();
        let db = super::super::worker::project_db_path(indexes.path(), project.id);
        let config = RuntimeConfig::from_config(&Config::default());
        let runtime = IndexRuntime::start_with_registrations_and_bus_and_gate(
            indexes.path().to_path_buf(),
            vec![project.clone()],
            Vec::new(),
            &[],
            None,
            Arc::clone(&config.codegraph_indexing_enabled),
        )
        .unwrap();
        assert!(!config.codegraph_indexing_enabled.load(Ordering::Acquire));
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(!db.exists());

        config
            .update_field("codegraph_indexing_enabled", &serde_json::json!(true))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if runtime
                    .handle()
                    .status(workspace.workspace_id())
                    .is_some_and(|status| status.phase == super::super::IndexPhase::Ready)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let ready = rsi_codegraph::CodegraphStore::open(&db, project.id)
            .unwrap()
            .current_ready(workspace.workspace_id())
            .unwrap();
        crate::store::daemon_settings::persist_runtime_config_field(
            &settings_store,
            &config,
            "codegraph_indexing_enabled",
        )
        .unwrap();
        drop(runtime);
        drop(settings_store);

        std::fs::write(root.path().join("lib.rs"), "pub fn second() {}\n").unwrap();
        let settings_store = Store::open(&settings_path).unwrap();
        let restarted = RuntimeConfig::from_config(&Config::default());
        crate::store::daemon_settings::apply_persisted_runtime_config(&settings_store, &restarted)
            .unwrap();
        assert!(restarted.codegraph_indexing_enabled.load(Ordering::Acquire));
        let runtime = IndexRuntime::start_with_registrations_and_bus_and_gate(
            indexes.path().to_path_buf(),
            vec![project.clone()],
            Vec::new(),
            &[],
            None,
            Arc::clone(&restarted.codegraph_indexing_enabled),
        )
        .unwrap();
        let successor = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let current = rsi_codegraph::CodegraphStore::open(&db, project.id)
                    .unwrap()
                    .current_ready(workspace.workspace_id())
                    .unwrap();
                if current != ready {
                    break current;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        restarted
            .update_field("codegraph_indexing_enabled", &serde_json::json!(false))
            .unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn third() {}\n").unwrap();
        runtime.handle().request(workspace.workspace_id()).unwrap();
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(
            rsi_codegraph::CodegraphStore::open(&db, project.id)
                .unwrap()
                .current_ready(workspace.workspace_id())
                .unwrap(),
            successor
        );
    }

    #[tokio::test]
    async fn registered_projects_start_without_external_session_paths() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn indexed() {}\n").unwrap();
        let project = Project {
            id: Uuid::new_v4(),
            name: "test".into(),
            path: Some(root.path().to_path_buf()),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let runtime =
            IndexRuntime::start(indexes.path().to_path_buf(), vec![project.clone()], &[]).unwrap();
        runtime.handle().set_enabled(true);
        let workspace = RegisteredWorkspace::primary(project.id, root.path()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if runtime
                    .handle()
                    .status(workspace.workspace_id())
                    .is_some_and(|status| status.phase == super::super::IndexPhase::Ready)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let outside = tempfile::tempdir().unwrap();
        let denied = IndexRuntime::start(
            indexes.path().to_path_buf(),
            vec![project],
            &[outside.path().to_path_buf()],
        )
        .unwrap();
        assert!(denied.handle().status(workspace.workspace_id()).is_none());
    }

    #[tokio::test]
    async fn verified_custody_registration_indexes_and_project_removal_deactivates() {
        let project_root = tempfile::tempdir().unwrap();
        let sandbox_root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::create_dir(project_root.path().join(".git")).unwrap();
        std::fs::write(
            sandbox_root.path().join(".git"),
            format!("gitdir: {}\n", project_root.path().join(".git").display()),
        )
        .unwrap();
        std::fs::write(project_root.path().join("lib.rs"), "pub fn primary() {}\n").unwrap();
        std::fs::write(sandbox_root.path().join("lib.rs"), "pub fn sandbox() {}\n").unwrap();
        let project = Project {
            id: Uuid::new_v4(),
            name: "registered".into(),
            path: Some(project_root.path().to_path_buf()),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let custody_id = Uuid::new_v4();
        let workspace = RegisteredWorkspace::registered_checkout(
            project.id,
            project_root.path(),
            sandbox_root.path(),
            rsi_codegraph::WorkspaceInstanceKey::RsiSandbox(custody_id),
        )
        .unwrap();
        let runtime = IndexRuntime::start_with_registrations(
            indexes.path().to_path_buf(),
            vec![project],
            vec![(
                custody_id,
                project_root.path().to_path_buf(),
                sandbox_root.path().to_path_buf(),
            )],
            &[],
        )
        .unwrap();
        runtime.handle().set_enabled(true);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if runtime
                    .handle()
                    .status(workspace.workspace_id())
                    .is_some_and(|status| status.phase == super::super::IndexPhase::Ready)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        runtime.handle().reconcile(Vec::new()).unwrap();
        assert!(runtime.handle().status(workspace.workspace_id()).is_none());
    }

    #[tokio::test]
    async fn project_created_after_start_is_registered() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn later() {}\n").unwrap();
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let mut runtime = IndexRuntime::start(indexes.path().to_path_buf(), vec![], &[]).unwrap();
        runtime.handle().set_enabled(true);
        runtime.attach_registry(store.clone(), Vec::new());
        let project = Project {
            id: Uuid::new_v4(),
            name: "later".into(),
            path: Some(root.path().to_path_buf()),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.lock().await.insert_project(&project).unwrap();
        let workspace = RegisteredWorkspace::primary(project.id, root.path()).unwrap();
        tokio::time::timeout(Duration::from_secs(7), async {
            loop {
                if runtime
                    .handle()
                    .status(workspace.workspace_id())
                    .is_some_and(|status| status.phase == super::super::IndexPhase::Ready)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        store.lock().await.delete_project(project.id).unwrap();
        tokio::time::timeout(Duration::from_secs(7), async {
            loop {
                if runtime.handle().status(workspace.workspace_id()).is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn custody_uses_longest_registered_project_prefix() {
        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        let checkout = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(
            checkout.path().join(".git"),
            format!("gitdir: {}\n", repo.join(".git").display()),
        )
        .unwrap();
        let outer_id = Uuid::new_v4();
        let inner_id = Uuid::new_v4();
        let now = Utc::now();
        let project = |id, path: PathBuf| Project {
            id,
            name: "registered".into(),
            path: Some(path),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        let custody_id = Uuid::new_v4();
        let workspaces = registered_workspaces(
            vec![
                project(outer_id, parent.path().to_path_buf()),
                project(inner_id, repo.clone()),
            ],
            vec![(custody_id, repo, checkout.path().to_path_buf())],
            &[],
        );
        let sandbox_id = rsi_codegraph::CodegraphStore::workspace_id(
            inner_id,
            &format!("rsi-sandbox:{custody_id}"),
        )
        .unwrap();
        assert!(
            workspaces
                .iter()
                .any(|workspace| workspace.workspace_id() == sandbox_id)
        );
        assert_eq!(workspaces.len(), 3);
    }
}
