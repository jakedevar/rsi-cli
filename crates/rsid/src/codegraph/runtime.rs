//! Daemon registry adapter. Projects and verified sandbox custody come from
//! the main Store; Git's exact worktree list admits contained detached roots.
//! Session and agent supplied paths never register workspaces.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rsi_common::types::Project;
use tokio::task::JoinHandle;
use tracing::warn;
use uuid::Uuid;

use super::{IndexHandle, IndexManager, IndexWatcher, RegisteredWorkspace, Result};
use crate::bus::EventBus;
use crate::sandbox::git_worktree::{MAX_GIT_WORKTREE_ENTRIES, discover_registered_worktree_roots};
use crate::store::Store;

type SandboxRegistration = (Uuid, PathBuf, PathBuf);
const REGISTRY_REFRESH: Duration = Duration::from_secs(5);
// The manager accepts at most 128 registered workspaces in one snapshot.
const MAX_INDEXED_WORKSPACES: usize = 128;
const REGISTRY_PAGE_SIZE: usize = 64;
const MAX_REGISTRY_CANDIDATES: usize = MAX_GIT_WORKTREE_ENTRIES;
const REGISTRY_SCAN_LIMIT: Duration = Duration::from_secs(30);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RegistryDiagnostics {
    unavailable_projects: usize,
    unavailable_sandboxes: usize,
    detached_failures: usize,
    ambiguous_detached: usize,
    changed_detached: usize,
    omitted_detached: usize,
    first_failure: Option<String>,
}

impl RegistryDiagnostics {
    fn note_failure(&mut self, error: impl ToString) {
        if self.first_failure.is_none() {
            self.first_failure = Some(error.to_string());
        }
    }

    fn report_if_changed(&self, previous: &mut Self) {
        if self != previous {
            if self != &Self::default() {
                warn!(
                    unavailable_projects = self.unavailable_projects,
                    unavailable_sandboxes = self.unavailable_sandboxes,
                    detached_failures = self.detached_failures,
                    ambiguous_detached = self.ambiguous_detached,
                    changed_detached = self.changed_detached,
                    omitted_detached = self.omitted_detached,
                    first_failure = self.first_failure.as_deref().unwrap_or(""),
                    "Codegraph registry admissions degraded"
                );
            }
            *previous = self.clone();
        }
    }
}

#[derive(Clone, Default)]
struct KnownRoots {
    roots: HashSet<PathBuf>,
    last_rowid: i64,
    detached: Vec<RegisteredWorkspace>,
    detached_scan: DetachedScanCache,
}

#[derive(Clone, Copy)]
enum RegistryStoreSource<'a> {
    Direct(&'a Store),
    Shared(&'a Arc<tokio::sync::Mutex<Store>>),
}

impl RegistryStoreSource<'_> {
    fn with<T>(&self, operation: impl FnOnce(&Store) -> T) -> T {
        match self {
            Self::Direct(store) => operation(store),
            Self::Shared(store) => operation(&store.blocking_lock()),
        }
    }
}

#[derive(Clone, Default)]
struct DetachedScanCache {
    active_signature: Vec<(Uuid, PathBuf)>,
    known_signature: Vec<PathBuf>,
    allowed_signature: Vec<PathBuf>,
    records: HashMap<Uuid, DetachedScanRecord>,
    omitted: usize,
}

#[derive(Clone)]
struct DetachedScanRecord {
    stamp: Option<blake3::Hash>,
    scanned_at: Instant,
    last_error: Option<String>,
}

fn hash_registration_file(
    hasher: &mut blake3::Hasher,
    path: &Path,
    content_matters: bool,
) -> Option<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hasher.update(&[0]);
            return Some(());
        }
        Err(_) => return None,
    };
    hasher.update(&[1]);
    hasher.update(&metadata.len().to_be_bytes());
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    hasher.update(&modified.as_secs().to_be_bytes());
    hasher.update(&modified.subsec_nanos().to_be_bytes());
    if content_matters {
        if metadata.len() > 4096 {
            return None;
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(4097)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > 4096 {
            return None;
        }
        hasher.update(&(bytes.len() as u64).to_be_bytes());
        hasher.update(&bytes);
    }
    Some(())
}

/// A cheap, bounded fingerprint of Git's registration records. Existing
/// `worktrees/<name>/gitdir` files may change in place without changing the
/// parent directory mtime, so the directory timestamp alone is insufficient.
fn git_registration_stamp(root: &Path) -> Option<blake3::Hash> {
    let common_dir = super::git_common_dir(root).ok()?;
    let mut hasher = blake3::Hasher::new();
    hash_registration_file(&mut hasher, &common_dir.join("config"), false)?;
    let worktrees = common_dir.join("worktrees");
    let entries = match std::fs::read_dir(&worktrees) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(hasher.finalize());
        }
        Err(_) => return None,
    };
    let mut paths = Vec::new();
    for entry in entries {
        paths.push(entry.ok()?.path());
        if paths.len() > MAX_GIT_WORKTREE_ENTRIES {
            return None;
        }
    }
    paths.sort();
    for entry in paths {
        hasher.update(entry.file_name()?.as_bytes());
        for name in ["gitdir", "commondir", "locked", "prunable"] {
            hasher.update(name.as_bytes());
            hash_registration_file(
                &mut hasher,
                &entry.join(name),
                matches!(name, "gitdir" | "commondir"),
            )?;
        }
    }
    Some(hasher.finalize())
}

fn detached_slots(workspaces: &[RegisteredWorkspace]) -> Vec<RegisteredWorkspace> {
    workspaces
        .iter()
        .filter(|workspace| {
            matches!(
                workspace.instance,
                rsi_codegraph::WorkspaceInstanceKey::DetachedRootHash(_)
            )
        })
        .cloned()
        .collect()
}

pub struct IndexRuntime {
    handle: IndexHandle,
    watchers: HashMap<Uuid, (PathBuf, IndexWatcher)>,
    task: JoinHandle<()>,
    registry_task: Option<JoinHandle<()>>,
    known_roots: KnownRoots,
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
        Self::start_with_workspaces(
            index_root,
            workspaces,
            events,
            enabled,
            KnownRoots::default(),
        )
    }

    /// Startup uses the same trusted admission builder as periodic refresh.
    /// Historical custody is retained in Store for reads but consumes no slot.
    pub fn start_from_store(
        index_root: PathBuf,
        store: &Store,
        allowed_roots: &[PathBuf],
        events: Option<Arc<EventBus>>,
        enabled: Arc<AtomicBool>,
    ) -> Result<Self> {
        let mut known_roots = KnownRoots::default();
        let (workspaces, diagnostics) =
            registry_snapshot_reported(store, allowed_roots, &mut known_roots)?;
        diagnostics.report_if_changed(&mut RegistryDiagnostics::default());
        Self::start_with_workspaces(index_root, workspaces, events, enabled, known_roots)
    }

    fn start_with_workspaces(
        index_root: PathBuf,
        workspaces: Vec<RegisteredWorkspace>,
        events: Option<Arc<EventBus>>,
        enabled: Arc<AtomicBool>,
        known_roots: KnownRoots,
    ) -> Result<Self> {
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
            known_roots,
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
        let mut known_roots = std::mem::take(&mut self.known_roots);
        self.registry_task = Some(tokio::spawn(async move {
            let mut watchers = initial_watchers;
            let mut prior_diagnostics = RegistryDiagnostics::default();
            let mut prior_error: Option<String> = None;
            let mut tick = tokio::time::interval(REGISTRY_REFRESH);
            loop {
                tick.tick().await;
                let source = Arc::clone(&store);
                let roots = allowed_roots.clone();
                let mut next_known_roots = known_roots.clone();
                let snapshot = tokio::task::spawn_blocking(move || {
                    let result = registry_snapshot_with_source(
                        RegistryStoreSource::Shared(&source),
                        &roots,
                        &mut next_known_roots,
                        || {},
                    );
                    (result, next_known_roots)
                })
                .await;
                let (workspaces, diagnostics, staged_cache) = match snapshot {
                    Ok((Ok((snapshot, diagnostics)), cache)) => (snapshot, diagnostics, cache),
                    Ok((Err(error), _)) => {
                        let message = error.to_string();
                        if prior_error.as_deref() != Some(&message) {
                            warn!(%error, "Codegraph registry refresh failed; prior registration retained");
                            prior_error = Some(message);
                        }
                        continue;
                    }
                    Err(error) => {
                        let message = error.to_string();
                        if prior_error.as_deref() != Some(&message) {
                            warn!(%error, "Codegraph registry worker stopped; prior registration retained");
                            prior_error = Some(message);
                        }
                        continue;
                    }
                };
                if let Err(error) = reconcile_registry_snapshot(
                    &handle,
                    workspaces.clone(),
                    &mut known_roots,
                    staged_cache,
                ) {
                    let message = error.to_string();
                    if prior_error.as_deref() != Some(&message) {
                        warn!(%error, "Codegraph registry snapshot rejected; prior registration retained");
                        prior_error = Some(message);
                    }
                    continue;
                }
                prior_error = None;
                diagnostics.report_if_changed(&mut prior_diagnostics);
                watchers = watch(&workspaces, &handle, watchers);
            }
        }));
    }

    pub fn handle(&self) -> &IndexHandle {
        &self.handle
    }
}

fn registry_snapshot_reported(
    store: &Store,
    allowed_roots: &[PathBuf],
    known_roots: &mut KnownRoots,
) -> Result<(Vec<RegisteredWorkspace>, RegistryDiagnostics)> {
    registry_snapshot_with_source(
        RegistryStoreSource::Direct(store),
        allowed_roots,
        known_roots,
        || {},
    )
}

fn reconcile_registry_snapshot(
    handle: &IndexHandle,
    workspaces: Vec<RegisteredWorkspace>,
    committed: &mut KnownRoots,
    staged: KnownRoots,
) -> Result<()> {
    handle.reconcile(workspaces)?;
    *committed = staged;
    Ok(())
}

/// All Store pages are fenced against one authorization observation. Each
/// page holds the shared Store only for SQLite work; root and Git validation
/// happens outside that lock. A changed Store observation restarts the entire
/// candidate traversal, never a partially mixed registry.
fn registry_snapshot_with_source(
    source: RegistryStoreSource<'_>,
    allowed_roots: &[PathBuf],
    known_roots: &mut KnownRoots,
    mut after_page: impl FnMut(),
) -> Result<(Vec<RegisteredWorkspace>, RegistryDiagnostics)> {
    let started = Instant::now();
    for _ in 0..3 {
        if started.elapsed() > REGISTRY_SCAN_LIMIT {
            return Err(super::IndexError::DiscoveryLimit("registry scan time"));
        }
        let mut staged = known_roots.clone();
        let (mut fence, projects) = source
            .with(|store| -> crate::error::Result<_> {
                let fence = store.codegraph_read_fence()?;
                Ok((fence, store.load_projects()?))
            })
            .map_err(|error| super::IndexError::UnsafeWorkspace(error.to_string()))?;
        if projects.len() > MAX_REGISTRY_CANDIDATES {
            return Err(super::IndexError::DiscoveryLimit("project candidates"));
        }
        let mut precheck = RegistryDiagnostics::default();
        let primaries = primary_workspaces_reported(
            &projects,
            allowed_roots,
            &mut precheck,
            MAX_INDEXED_WORKSPACES,
        )?;
        if started.elapsed() > REGISTRY_SCAN_LIMIT {
            return Err(super::IndexError::DiscoveryLimit("registry scan time"));
        }
        let project_roots = primaries
            .iter()
            .map(|primary| (primary.root().to_path_buf(), primary.project_id()))
            .collect::<Vec<_>>();

        let mut changed = false;
        let mut scanned_known = 0usize;
        loop {
            let page = source
                .with(|store| -> crate::error::Result<_> {
                    if !store.codegraph_read_fence_matches(&mut fence)? {
                        return Ok(None);
                    }
                    Ok(Some(store.codegraph_known_roots_since(
                        staged.last_rowid,
                        REGISTRY_PAGE_SIZE,
                    )?))
                })
                .map_err(|error| super::IndexError::UnsafeWorkspace(error.to_string()))?;
            let Some(page) = page else {
                changed = true;
                break;
            };
            if page.is_empty() {
                break;
            }
            for (rowid, root) in page {
                scanned_known += 1;
                if scanned_known > MAX_REGISTRY_CANDIDATES
                    || started.elapsed() > REGISTRY_SCAN_LIMIT
                {
                    return Err(super::IndexError::DiscoveryLimit("known custody roots"));
                }
                staged.roots.insert(root);
                if staged.roots.len() > MAX_REGISTRY_CANDIDATES {
                    return Err(super::IndexError::DiscoveryLimit("known custody roots"));
                }
                staged.last_rowid = rowid;
            }
            after_page();
        }
        if changed {
            continue;
        }

        let mut sandboxes = Vec::new();
        let mut after = None;
        let mut scanned_active = 0usize;
        loop {
            let page = source
                .with(|store| -> crate::error::Result<_> {
                    if !store.codegraph_read_fence_matches(&mut fence)? {
                        return Ok(None);
                    }
                    Ok(Some(store.codegraph_custody_page(
                        after,
                        true,
                        REGISTRY_PAGE_SIZE,
                    )?))
                })
                .map_err(|error| super::IndexError::UnsafeWorkspace(error.to_string()))?;
            let Some(page) = page else {
                changed = true;
                break;
            };
            if page.is_empty() {
                break;
            }
            for (custody_id, _, repo, root, _) in page {
                scanned_active += 1;
                if scanned_active > MAX_REGISTRY_CANDIDATES
                    || started.elapsed() > REGISTRY_SCAN_LIMIT
                {
                    return Err(super::IndexError::DiscoveryLimit(
                        "active custody candidates",
                    ));
                }
                after = Some(custody_id);
                let candidate = (custody_id, repo, root);
                match eligible_sandbox(&project_roots, &candidate) {
                    Ok(Some(_)) => {
                        sandboxes.push(candidate);
                        if primaries.len() + sandboxes.len() > MAX_INDEXED_WORKSPACES {
                            return Err(super::IndexError::DiscoveryLimit(
                                "active registered workspaces",
                            ));
                        }
                    }
                    Ok(None) => precheck.unavailable_sandboxes += 1,
                    Err(error) => {
                        precheck.unavailable_sandboxes += 1;
                        precheck.note_failure(error);
                    }
                }
            }
            after_page();
        }
        if changed {
            continue;
        }

        let stable = source
            .with(|store| store.codegraph_read_fence_matches(&mut fence))
            .map_err(|error| super::IndexError::UnsafeWorkspace(error.to_string()))?;
        if !stable {
            continue;
        }

        let (workspaces, mut diagnostics) = build_registry_with_scan_cache(
            projects,
            sandboxes,
            allowed_roots,
            &staged.roots,
            &staged.detached,
            Some(&mut staged.detached_scan),
        )?;
        if started.elapsed() > REGISTRY_SCAN_LIMIT {
            return Err(super::IndexError::DiscoveryLimit("registry scan time"));
        }
        let stable = source
            .with(|store| store.codegraph_read_fence_matches(&mut fence))
            .map_err(|error| super::IndexError::UnsafeWorkspace(error.to_string()))?;
        if !stable {
            continue;
        }
        diagnostics.unavailable_sandboxes += precheck.unavailable_sandboxes;
        if diagnostics.first_failure.is_none() {
            diagnostics.first_failure = precheck.first_failure;
        }
        staged.detached = detached_slots(&workspaces);
        *known_roots = staged;
        return Ok((workspaces, diagnostics));
    }
    Err(super::IndexError::UnsafeWorkspace(
        "Codegraph Store registry changed during bounded snapshot".into(),
    ))
}

fn build_registry(
    projects: Vec<Project>,
    sandboxes: Vec<SandboxRegistration>,
    allowed_roots: &[PathBuf],
    known_roots: &HashSet<PathBuf>,
) -> Result<Vec<RegisteredWorkspace>> {
    build_registry_reported(projects, sandboxes, allowed_roots, known_roots)
        .map(|(workspaces, _)| workspaces)
}

fn build_registry_reported(
    projects: Vec<Project>,
    sandboxes: Vec<SandboxRegistration>,
    allowed_roots: &[PathBuf],
    known_roots: &HashSet<PathBuf>,
) -> Result<(Vec<RegisteredWorkspace>, RegistryDiagnostics)> {
    build_registry_with_cache(projects, sandboxes, allowed_roots, known_roots, &[])
}

fn build_registry_with_cache(
    projects: Vec<Project>,
    sandboxes: Vec<SandboxRegistration>,
    allowed_roots: &[PathBuf],
    known_roots: &HashSet<PathBuf>,
    previous_detached: &[RegisteredWorkspace],
) -> Result<(Vec<RegisteredWorkspace>, RegistryDiagnostics)> {
    build_registry_with_scan_cache(
        projects,
        sandboxes,
        allowed_roots,
        known_roots,
        previous_detached,
        None,
    )
}

fn build_registry_with_scan_cache(
    projects: Vec<Project>,
    sandboxes: Vec<SandboxRegistration>,
    allowed_roots: &[PathBuf],
    known_roots: &HashSet<PathBuf>,
    previous_detached: &[RegisteredWorkspace],
    scan_cache: Option<&mut DetachedScanCache>,
) -> Result<(Vec<RegisteredWorkspace>, RegistryDiagnostics)> {
    let mut diagnostics = RegistryDiagnostics::default();
    let workspaces = registered_workspaces_with_detached_reported(
        projects,
        sandboxes,
        allowed_roots,
        known_roots,
        previous_detached,
        scan_cache,
        &mut diagnostics,
    );
    if workspaces.len() > MAX_INDEXED_WORKSPACES {
        return Err(super::IndexError::DiscoveryLimit(
            "active registered workspaces",
        ));
    }
    Ok((workspaces, diagnostics))
}

fn registered_workspaces(
    projects: Vec<Project>,
    sandboxes: Vec<SandboxRegistration>,
    allowed_roots: &[PathBuf],
) -> Vec<RegisteredWorkspace> {
    registered_workspaces_reported(
        projects,
        sandboxes,
        allowed_roots,
        &mut RegistryDiagnostics::default(),
    )
}

fn registered_workspaces_reported(
    projects: Vec<Project>,
    sandboxes: Vec<SandboxRegistration>,
    allowed_roots: &[PathBuf],
    diagnostics: &mut RegistryDiagnostics,
) -> Vec<RegisteredWorkspace> {
    let mut workspaces =
        primary_workspaces_reported(&projects, allowed_roots, diagnostics, usize::MAX)
            .expect("unbounded primary collection cannot overflow");
    let project_roots = workspaces
        .iter()
        .map(|workspace| (workspace.root().to_path_buf(), workspace.project_id()))
        .collect::<Vec<_>>();
    for sandbox in sandboxes {
        match eligible_sandbox(&project_roots, &sandbox) {
            Ok(Some(workspace)) => workspaces.push(workspace),
            Ok(None) => diagnostics.unavailable_sandboxes += 1,
            Err(error) => {
                diagnostics.unavailable_sandboxes += 1;
                diagnostics.note_failure(error);
            }
        }
    }
    workspaces
}

fn primary_workspaces_reported(
    projects: &[Project],
    allowed_roots: &[PathBuf],
    diagnostics: &mut RegistryDiagnostics,
    max_accepted: usize,
) -> Result<Vec<RegisteredWorkspace>> {
    let mut workspaces = Vec::new();
    for project in projects {
        let Some(path) = project.path.as_ref() else {
            continue;
        };
        match RegisteredWorkspace::primary(project.id, &path) {
            Ok(workspace) if allowed(&workspace, allowed_roots) => {
                workspaces.push(workspace);
                if workspaces.len() > max_accepted {
                    return Err(super::IndexError::DiscoveryLimit(
                        "active registered workspaces",
                    ));
                }
            }
            Ok(_) => diagnostics.unavailable_projects += 1,
            Err(error) => {
                diagnostics.unavailable_projects += 1;
                diagnostics.note_failure(error);
            }
        }
    }
    Ok(workspaces)
}

fn eligible_sandbox(
    project_roots: &[(PathBuf, Uuid)],
    sandbox: &SandboxRegistration,
) -> Result<Option<RegisteredWorkspace>> {
    let (custody_id, repo, root) = sandbox;
    if repo.canonicalize().ok().as_ref() != Some(repo) {
        return Ok(None);
    }
    // The registered project may contain the source checkout rather than
    // equal it. Use the same longest-prefix rule as project resolution.
    let longest = project_roots
        .iter()
        .filter(|(path, _)| repo.starts_with(path))
        .map(|(path, _)| path.components().count())
        .max();
    let Some(longest) = longest else {
        return Ok(None);
    };
    let matches = project_roots
        .iter()
        .filter(|(path, _)| repo.starts_with(path) && path.components().count() == longest)
        .collect::<Vec<_>>();
    // Identical registered paths under distinct project IDs are ambiguous.
    let [(_, project_id)] = matches.as_slice() else {
        return Ok(None);
    };
    let workspace = RegisteredWorkspace::registered_checkout(
        *project_id,
        repo,
        root,
        rsi_codegraph::WorkspaceInstanceKey::RsiSandbox(*custody_id),
    )?;
    Ok((workspace.root() == root).then_some(workspace))
}

/// Detached admission comes only from Git's registered worktree list. Keep
/// this entire read on the blocking registry worker, never the async tick.
fn registered_workspaces_with_detached(
    projects: Vec<Project>,
    sandboxes: Vec<SandboxRegistration>,
    allowed_roots: &[PathBuf],
) -> Vec<RegisteredWorkspace> {
    registered_workspaces_with_detached_excluding(
        projects,
        sandboxes,
        allowed_roots,
        &HashSet::new(),
    )
}

fn registered_workspaces_with_detached_excluding(
    projects: Vec<Project>,
    sandboxes: Vec<SandboxRegistration>,
    allowed_roots: &[PathBuf],
    known_roots: &HashSet<PathBuf>,
) -> Vec<RegisteredWorkspace> {
    registered_workspaces_with_detached_reported(
        projects,
        sandboxes,
        allowed_roots,
        known_roots,
        &[],
        None,
        &mut RegistryDiagnostics::default(),
    )
}

fn registered_workspaces_with_detached_reported(
    projects: Vec<Project>,
    sandboxes: Vec<SandboxRegistration>,
    allowed_roots: &[PathBuf],
    known_roots: &HashSet<PathBuf>,
    previous_detached: &[RegisteredWorkspace],
    mut scan_cache: Option<&mut DetachedScanCache>,
    diagnostics: &mut RegistryDiagnostics,
) -> Vec<RegisteredWorkspace> {
    let mut workspaces =
        registered_workspaces_reported(projects, sandboxes, allowed_roots, diagnostics);
    let mut active_signature = workspaces
        .iter()
        .map(|workspace| (workspace.workspace_id(), workspace.root().to_path_buf()))
        .collect::<Vec<_>>();
    active_signature.sort();
    let mut known_signature = known_roots.iter().cloned().collect::<Vec<_>>();
    known_signature.sort();
    let mut allowed_signature = allowed_roots.to_vec();
    allowed_signature.sort();
    if allowed_roots.is_empty() || workspaces.len() >= MAX_INDEXED_WORKSPACES {
        if let Some(cache) = scan_cache.as_deref_mut() {
            cache.active_signature = active_signature;
            cache.known_signature = known_signature;
            cache.allowed_signature = allowed_signature;
            cache.omitted = 0;
        }
        return workspaces;
    }

    let primaries = workspaces
        .iter()
        .filter(|workspace| {
            rsi_codegraph::CodegraphStore::workspace_id(workspace.project_id(), "primary")
                .is_ok_and(|id| id == workspace.workspace_id())
                && workspace.root().join(".git").exists()
        })
        .collect::<Vec<_>>();
    let use_cache = scan_cache.as_ref().is_some_and(|cache| {
        cache.active_signature == active_signature
            && cache.known_signature == known_signature
            && cache.allowed_signature == allowed_signature
            && primaries.iter().all(|primary| {
                let stamp = git_registration_stamp(primary.root());
                cache
                    .records
                    .get(&primary.project_id())
                    .is_some_and(|record| {
                        stamp.is_some()
                            && record.stamp == stamp
                            && record.scanned_at.elapsed() < Duration::from_secs(60)
                    })
            })
    });
    let mut refreshed_records = HashMap::new();

    let registered_roots: HashSet<PathBuf> = workspaces
        .iter()
        .map(|workspace| workspace.root().to_path_buf())
        .collect();
    let mut candidates: HashMap<PathBuf, Vec<(Uuid, PathBuf)>> = HashMap::new();
    for primary in primaries {
        let roots = if use_cache {
            if let Some(error) = scan_cache
                .as_ref()
                .and_then(|cache| cache.records.get(&primary.project_id()))
                .and_then(|record| record.last_error.as_ref())
            {
                diagnostics.detached_failures += 1;
                diagnostics.note_failure(error);
            }
            previous_detached
                .iter()
                .filter(|prior| prior.project_id() == primary.project_id())
                .map(|prior| prior.root().to_path_buf())
                .collect()
        } else {
            let stamp = git_registration_stamp(primary.root());
            match discover_registered_worktree_roots(
                primary.root(),
                MAX_GIT_WORKTREE_ENTRIES,
                known_roots,
            ) {
                Ok(roots) => {
                    refreshed_records.insert(
                        primary.project_id(),
                        DetachedScanRecord {
                            stamp,
                            scanned_at: Instant::now(),
                            last_error: None,
                        },
                    );
                    roots
                }
                Err(error) => {
                    let message = error.to_string();
                    diagnostics.detached_failures += 1;
                    diagnostics.note_failure(&message);
                    refreshed_records.insert(
                        primary.project_id(),
                        DetachedScanRecord {
                            stamp,
                            scanned_at: Instant::now(),
                            last_error: Some(message),
                        },
                    );
                    previous_detached
                        .iter()
                        .filter(|prior| prior.project_id() == primary.project_id())
                        .map(|prior| prior.root().to_path_buf())
                        .collect()
                }
            }
        };
        for root in roots {
            if !registered_roots.contains(&root)
                && !known_roots.contains(&root)
                && allowed_roots.iter().any(|allowed| contains(allowed, &root))
            {
                candidates
                    .entry(root)
                    .or_default()
                    .push((primary.project_id(), primary.root().to_path_buf()));
            }
        }
    }
    let mut candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    let mut omitted = 0usize;
    for (root, owners) in candidates {
        let [(project_id, project_root)] = owners.as_slice() else {
            diagnostics.ambiguous_detached += 1;
            continue;
        };
        match RegisteredWorkspace::detached_checkout(*project_id, project_root, &root) {
            Ok(workspace)
                if workspace.root() == root
                    && allowed_roots
                        .iter()
                        .any(|allowed| contains(allowed, workspace.root())) =>
            {
                if workspaces.len() < MAX_INDEXED_WORKSPACES {
                    workspaces.push(workspace);
                } else {
                    omitted += 1;
                }
            }
            Ok(_) => {
                diagnostics.changed_detached += 1;
            }
            Err(error) => {
                diagnostics.changed_detached += 1;
                diagnostics.note_failure(error);
            }
        }
    }
    if let Some(cache) = scan_cache.as_deref_mut() {
        if use_cache {
            diagnostics.omitted_detached = cache.omitted;
        } else {
            cache.records = refreshed_records;
            cache.active_signature = active_signature;
            cache.known_signature = known_signature;
            cache.allowed_signature = allowed_signature;
            cache.omitted = omitted;
            diagnostics.omitted_detached = omitted;
        }
    } else {
        diagnostics.omitted_detached = omitted;
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
    use rsid_store::test_support::make_test_session;
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

    fn stored_custody(
        store: &Store,
        external: &Store,
        project_id: Uuid,
        ordinal: u128,
        repo: &Path,
        root: &Path,
    ) -> Uuid {
        let mut session = make_test_session();
        session.id = Uuid::from_u128(ordinal + 1);
        session.project_id = Some(project_id);
        session.status = rsi_common::types::SessionStatus::Running;
        session.working_dir = root.to_path_buf();
        store.insert_session(&session).unwrap();
        let custody_id = Uuid::from_u128(ordinal + 1000);
        external
            .insert_codegraph_custody_fixture(custody_id, session.id, repo, root)
            .unwrap();
        custody_id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn active_registry_finds_eligible_custody_after_129_invalid_candidates() {
        let (base, repo) = git_fixture();
        let valid_root = base.path().join("valid");
        add_worktree(&repo, &valid_root);
        let project_id = Uuid::new_v4();
        let valid_id = Uuid::new_v4();
        let mut sandboxes = (0..129)
            .map(|ordinal| {
                (
                    Uuid::new_v4(),
                    base.path().join(format!("missing-repo-{ordinal}")),
                    base.path().join(format!("missing-root-{ordinal}")),
                )
            })
            .collect::<Vec<_>>();
        sandboxes.push((valid_id, repo.clone(), valid_root.clone()));
        let (workspaces, diagnostics) = build_registry_reported(
            vec![project(project_id, repo.clone())],
            sandboxes,
            &[],
            &HashSet::new(),
        )
        .unwrap();
        let valid = RegisteredWorkspace::registered_checkout(
            project_id,
            &repo,
            &valid_root,
            rsi_codegraph::WorkspaceInstanceKey::RsiSandbox(valid_id),
        )
        .unwrap();
        assert_eq!(workspaces.len(), 2);
        assert_eq!(diagnostics.unavailable_sandboxes, 129);
        assert!(
            workspaces
                .iter()
                .any(|item| item.workspace_id() == valid.workspace_id())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn store_paged_admission_finds_valid_root_after_129_ineligible_rows() {
        let base = tempfile::tempdir().unwrap();
        let repo = base.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let valid_root = base.path().join("valid");
        std::fs::create_dir(&valid_root).unwrap();
        std::fs::write(
            valid_root.join(".git"),
            format!("gitdir: {}\n", repo.join(".git").display()),
        )
        .unwrap();
        let database = base.path().join("store.db");
        let store = Store::open(&database).unwrap();
        let external = Store::open(&database).unwrap();
        let project_id = Uuid::new_v4();
        store
            .insert_project(&project(project_id, repo.clone()))
            .unwrap();
        for ordinal in 0..129_u128 {
            stored_custody(
                &store,
                &external,
                project_id,
                ordinal,
                &base.path().join(format!("missing-repo-{ordinal}")),
                &base.path().join(format!("missing-root-{ordinal}")),
            );
        }
        let valid_id = stored_custody(&store, &external, project_id, 129, &repo, &valid_root);
        let mut known = KnownRoots::default();
        let (workspaces, diagnostics) =
            registry_snapshot_reported(&store, &[], &mut known).unwrap();
        let valid_workspace = rsi_codegraph::CodegraphStore::workspace_id(
            project_id,
            &format!("rsi-sandbox:{valid_id}"),
        )
        .unwrap();
        assert_eq!(workspaces.len(), 2);
        assert_eq!(diagnostics.unavailable_sandboxes, 129);
        assert!(
            workspaces
                .iter()
                .any(|item| item.workspace_id() == valid_workspace)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn store_paged_admission_reports_129_eligible_slots() {
        let base = tempfile::tempdir().unwrap();
        let repo = base.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let database = base.path().join("store.db");
        let store = Store::open(&database).unwrap();
        let external = Store::open(&database).unwrap();
        let project_id = Uuid::new_v4();
        store
            .insert_project(&project(project_id, repo.clone()))
            .unwrap();
        for ordinal in 0..128_u128 {
            let root = base.path().join(format!("active-{ordinal:03}"));
            std::fs::create_dir(&root).unwrap();
            std::fs::write(
                root.join(".git"),
                format!("gitdir: {}\n", repo.join(".git").display()),
            )
            .unwrap();
            stored_custody(&store, &external, project_id, ordinal, &repo, &root);
        }
        let error =
            registry_snapshot_reported(&store, &[], &mut KnownRoots::default()).unwrap_err();
        assert!(matches!(
            error,
            super::super::IndexError::DiscoveryLimit("active registered workspaces")
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn store_paged_admission_restarts_when_custody_changes_between_pages() {
        let base = tempfile::tempdir().unwrap();
        let repo = base.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let valid_root = base.path().join("valid");
        std::fs::create_dir(&valid_root).unwrap();
        std::fs::write(
            valid_root.join(".git"),
            format!("gitdir: {}\n", repo.join(".git").display()),
        )
        .unwrap();
        let database = base.path().join("store.db");
        let store = Store::open(&database).unwrap();
        let external = Store::open(&database).unwrap();
        let project_id = Uuid::new_v4();
        store
            .insert_project(&project(project_id, repo.clone()))
            .unwrap();
        let first_id = stored_custody(&store, &external, project_id, 0, &repo, &valid_root);
        for ordinal in 1..65_u128 {
            stored_custody(
                &store,
                &external,
                project_id,
                ordinal,
                &base.path().join(format!("missing-repo-{ordinal}")),
                &base.path().join(format!("missing-root-{ordinal}")),
            );
        }
        let shared = Arc::new(tokio::sync::Mutex::new(store));
        let mut page_count = 0;
        let mut changed = false;
        let (workspaces, _) = registry_snapshot_with_source(
            RegistryStoreSource::Shared(&shared),
            &[],
            &mut KnownRoots::default(),
            || {
                page_count += 1;
                if page_count == 3 {
                    external
                        .update_session_status(
                            Uuid::from_u128(1),
                            rsi_common::types::SessionStatus::Completed,
                        )
                        .unwrap();
                    changed = true;
                }
            },
        )
        .unwrap();
        let revoked_workspace = rsi_codegraph::CodegraphStore::workspace_id(
            project_id,
            &format!("rsi-sandbox:{first_id}"),
        )
        .unwrap();
        assert!(changed);
        assert!(
            page_count >= 5,
            "snapshot did not traverse again after mutation"
        );
        assert_eq!(workspaces.len(), 1);
        assert!(
            workspaces
                .iter()
                .all(|item| item.workspace_id() != revoked_workspace)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn store_paged_admission_restarts_when_project_changes_between_pages() {
        let base = tempfile::tempdir().unwrap();
        let repo = base.path().join("repo");
        let replacement = base.path().join("replacement");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(replacement.join(".git")).unwrap();
        let root = base.path().join("active");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", repo.join(".git").display()),
        )
        .unwrap();
        let database = base.path().join("store.db");
        let store = Store::open(&database).unwrap();
        let external = Store::open(&database).unwrap();
        let project_id = Uuid::new_v4();
        store
            .insert_project(&project(project_id, repo.clone()))
            .unwrap();
        stored_custody(&store, &external, project_id, 0, &repo, &root);
        let shared = Arc::new(tokio::sync::Mutex::new(store));
        let mut page_count = 0;
        let (workspaces, _) = registry_snapshot_with_source(
            RegistryStoreSource::Shared(&shared),
            &[],
            &mut KnownRoots::default(),
            || {
                page_count += 1;
                if page_count == 1 {
                    external
                        .update_project(&project(project_id, replacement.clone()))
                        .unwrap();
                }
            },
        )
        .unwrap();
        assert!(
            page_count >= 2,
            "snapshot did not restart after project mutation"
        );
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].root(), replacement.as_path());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn rejected_reconcile_keeps_previous_registry_cache() {
        let root = tempfile::tempdir().unwrap();
        let index = tempfile::tempdir().unwrap();
        let project_id = Uuid::new_v4();
        let primary = RegisteredWorkspace::primary(project_id, root.path()).unwrap();
        let (_manager, handle) =
            IndexManager::new(index.path().to_path_buf(), vec![primary.clone()]).unwrap();
        let old_root = root.path().join("old-custody");
        let mut committed = KnownRoots::default();
        committed.roots.insert(old_root.clone());
        committed.last_rowid = 7;
        committed.detached_scan.active_signature = vec![(project_id, root.path().to_path_buf())];
        let mut staged = committed.clone();
        staged.roots.insert(root.path().join("new-custody"));
        staged.last_rowid = 8;
        staged.detached_scan.active_signature.clear();
        let error = reconcile_registry_snapshot(
            &handle,
            vec![primary.clone(), primary],
            &mut committed,
            staged,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            super::super::IndexError::UnsafeWorkspace(_)
        ));
        assert_eq!(committed.last_rowid, 7);
        assert_eq!(committed.roots.len(), 1);
        assert!(committed.roots.contains(&old_root));
        assert_eq!(committed.detached_scan.active_signature.len(), 1);
        assert_eq!(
            handle
                .registered_project_workspaces(project_id)
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn active_custody_symlink_alias_cannot_change_workspace_root() {
        let (base, repo) = git_fixture();
        let actual = base.path().join("actual");
        let alias = base.path().join("alias");
        add_worktree(&repo, &actual);
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        let (workspaces, diagnostics) = build_registry_reported(
            vec![project(Uuid::new_v4(), repo.clone())],
            vec![(Uuid::new_v4(), repo, alias)],
            &[],
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(diagnostics.unavailable_sandboxes, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn active_registry_reports_capacity_when_every_custody_is_eligible() {
        let (base, repo) = git_fixture();
        let mut sandboxes = Vec::new();
        for ordinal in 0..128 {
            let root = base.path().join(format!("active-{ordinal:03}"));
            add_worktree(&repo, &root);
            sandboxes.push((Uuid::new_v4(), repo.clone(), root));
        }
        let error = build_registry(
            vec![project(Uuid::new_v4(), repo)],
            sandboxes,
            &[],
            &HashSet::new(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            super::super::IndexError::DiscoveryLimit("active registered workspaces")
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn detached_discovery_skips_over_128_historical_rsi_roots() {
        let (base, repo) = git_fixture();
        let mut known = HashSet::new();
        for ordinal in 0..129 {
            let root = base.path().join(format!("historical-{ordinal:03}"));
            add_worktree(&repo, &root);
            known.insert(root.canonicalize().unwrap());
        }
        let ordinary = base.path().join("ordinary");
        add_worktree(&repo, &ordinary);
        let project_id = Uuid::new_v4();
        let workspaces = registered_workspaces_with_detached_excluding(
            vec![project(project_id, repo.clone())],
            Vec::new(),
            &[base.path().to_path_buf()],
            &known,
        );
        let detached =
            RegisteredWorkspace::detached_checkout(project_id, &repo, &ordinary).unwrap();
        assert_eq!(workspaces.len(), 2);
        assert!(
            workspaces
                .iter()
                .any(|item| item.workspace_id() == detached.workspace_id())
        );
        for root in known {
            assert!(workspaces.iter().all(|item| item.root() != root));
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn detached_discovery_failure_revalidates_prior_registration() {
        let (base, repo) = git_fixture();
        let detached_root = base.path().join("detached");
        add_worktree(&repo, &detached_root);
        let project_id = Uuid::new_v4();
        let allowed = vec![base.path().to_path_buf()];
        let (first, first_diagnostics) = build_registry_with_cache(
            vec![project(project_id, repo.clone())],
            Vec::new(),
            &allowed,
            &HashSet::new(),
            &[],
        )
        .unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first_diagnostics.detached_failures, 0);
        let cached = detached_slots(&first);
        assert_eq!(cached.len(), 1);
        std::fs::write(repo.join(".git/config"), "[invalid\n").unwrap();
        let (second, diagnostics) = build_registry_with_cache(
            vec![project(project_id, repo)],
            Vec::new(),
            &allowed,
            &HashSet::new(),
            &cached,
        )
        .unwrap();
        assert_eq!(diagnostics.detached_failures, 1);
        assert_eq!(second.len(), 2);
        assert_eq!(second[1].workspace_id(), cached[0].workspace_id());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn unchanged_git_registration_uses_bounded_scan_cadence() {
        let (base, repo) = git_fixture();
        let detached_root = base.path().join("detached");
        add_worktree(&repo, &detached_root);
        let project_id = Uuid::new_v4();
        let allowed = vec![base.path().to_path_buf()];
        let mut scan_cache = DetachedScanCache::default();
        let (first, diagnostics) = build_registry_with_scan_cache(
            vec![project(project_id, repo.clone())],
            Vec::new(),
            &allowed,
            &HashSet::new(),
            &[],
            Some(&mut scan_cache),
        )
        .unwrap();
        assert_eq!(diagnostics.detached_failures, 0);
        let cached = detached_slots(&first);
        assert_eq!(cached.len(), 1);
        let initial_scan = scan_cache.records.get(&project_id).unwrap().scanned_at;
        let (second, diagnostics) = build_registry_with_scan_cache(
            vec![project(project_id, repo.clone())],
            Vec::new(),
            &allowed,
            &HashSet::new(),
            &cached,
            Some(&mut scan_cache),
        )
        .unwrap();
        assert_eq!(diagnostics.detached_failures, 0);
        assert_eq!(second[1].workspace_id(), cached[0].workspace_id());
        assert_eq!(
            scan_cache.records.get(&project_id).unwrap().scanned_at,
            initial_scan
        );
        scan_cache.records.get_mut(&project_id).unwrap().scanned_at -= Duration::from_secs(61);
        let (third, diagnostics) = build_registry_with_scan_cache(
            vec![project(project_id, repo.clone())],
            Vec::new(),
            &allowed,
            &HashSet::new(),
            &cached,
            Some(&mut scan_cache),
        )
        .unwrap();
        assert_eq!(diagnostics.detached_failures, 0);
        assert_eq!(third[1].workspace_id(), cached[0].workspace_id());
        assert!(scan_cache.records.get(&project_id).unwrap().scanned_at > initial_scan);
        std::fs::write(repo.join(".git/config"), "[invalid\n").unwrap();
        let (_, diagnostics) = build_registry_with_scan_cache(
            vec![project(project_id, repo)],
            Vec::new(),
            &allowed,
            &HashSet::new(),
            &cached,
            Some(&mut scan_cache),
        )
        .unwrap();
        assert_eq!(diagnostics.detached_failures, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn new_known_custody_root_invalidates_detached_scan_cache() {
        let (base, repo) = git_fixture();
        let detached_root = base.path().join("detached");
        add_worktree(&repo, &detached_root);
        let project_id = Uuid::new_v4();
        let allowed = vec![base.path().to_path_buf()];
        let mut scan_cache = DetachedScanCache::default();
        let (first, _) = build_registry_with_scan_cache(
            vec![project(project_id, repo.clone())],
            Vec::new(),
            &allowed,
            &HashSet::new(),
            &[],
            Some(&mut scan_cache),
        )
        .unwrap();
        assert_eq!(first.len(), 2);
        std::fs::write(repo.join(".git/config"), "[invalid\n").unwrap();
        let known_roots = HashSet::from([detached_root]);
        let (second, diagnostics) = build_registry_with_scan_cache(
            vec![project(project_id, repo)],
            Vec::new(),
            &allowed,
            &known_roots,
            &detached_slots(&first),
            Some(&mut scan_cache),
        )
        .unwrap();
        assert_eq!(diagnostics.detached_failures, 1);
        assert_eq!(second.len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn changed_gitdir_registration_refreshes_detached_scan_immediately() {
        let (base, repo) = git_fixture();
        let detached_root = base.path().join("detached");
        add_worktree(&repo, &detached_root);
        let project_id = Uuid::new_v4();
        let allowed = vec![base.path().to_path_buf()];
        let mut scan_cache = DetachedScanCache::default();
        let (first, _) = build_registry_with_scan_cache(
            vec![project(project_id, repo.clone())],
            Vec::new(),
            &allowed,
            &HashSet::new(),
            &[],
            Some(&mut scan_cache),
        )
        .unwrap();
        assert_eq!(first.len(), 2);
        let prior_stamp = git_registration_stamp(&repo).unwrap();
        let registration = std::fs::read_dir(repo.join(".git/worktrees"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(registration.join("gitdir"), "/missing/registration/.git\n").unwrap();
        assert_ne!(git_registration_stamp(&repo).unwrap(), prior_stamp);
        let (second, _) = build_registry_with_scan_cache(
            vec![project(project_id, repo)],
            Vec::new(),
            &allowed,
            &HashSet::new(),
            &detached_slots(&first),
            Some(&mut scan_cache),
        )
        .unwrap();
        assert_eq!(second.len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
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
