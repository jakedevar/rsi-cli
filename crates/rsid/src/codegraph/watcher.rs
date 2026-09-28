//! Notify callbacks carry no source bytes or database effects. Every callback
//! collapses to a complete-workspace request; lost events are repaired by the
//! manager's periodic content scan.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{IndexError, RegisteredWorkspace, Result, manager::IndexHandle};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{info, warn};
use uuid::Uuid;

const MAX_WORKSPACE_WATCHES: usize = 1_024;
const MAX_TOTAL_WATCHES: usize = 8_192;
const MAX_PLAN_ENTRIES: usize = 32_768;
const IGNORED_DIRS: &[&str] = &[".git", ".rsi", "target", "node_modules", "vendor", ".venv"];
// Watch hints may omit these caches; the exact-content scanner remains the
// authority and retains its existing source policy.
const CACHE_DIRS: &[&str] = &[".cache", ".npm", ".cargo", ".rustup"];
static RESERVED_WATCHES: AtomicUsize = AtomicUsize::new(0);

/// Reserve before touching the backend, including while another watcher is
/// being replaced. Failed/partial installations conservatively keep the whole
/// reservation until their backend is dropped.
struct WatchReservation<'a> {
    used: &'a AtomicUsize,
    count: usize,
}

impl<'a> WatchReservation<'a> {
    fn acquire(used: &'a AtomicUsize, count: usize, limit: usize) -> Option<Self> {
        used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(count).filter(|next| *next <= limit)
        })
        .ok()
        .map(|_| Self { used, count })
    }
}

impl Drop for WatchReservation<'_> {
    fn drop(&mut self) {
        self.used.fetch_sub(self.count, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct WatchPlan {
    directories: Vec<PathBuf>,
    entries: usize,
}

fn watch_ignored(name: &std::ffi::OsStr) -> bool {
    IGNORED_DIRS
        .iter()
        .chain(CACHE_DIRS)
        .any(|ignored| name == *ignored)
}

fn checked_directory(path: &Path) -> Result<()> {
    // Recheck before descent AND registration. A rename can invalidate a plan.
    // Canonical equality also rejects substituted symlink ancestors. Notify's
    // path API cannot eliminate the final race, but nonrecursive watches keep
    // even a raced installation bounded to one directory.
    if !std::fs::symlink_metadata(path)?.file_type().is_dir() || path.canonicalize()? != path {
        return Err(IndexError::UnsafeWorkspace(
            "watch directory changed".into(),
        ));
    }
    Ok(())
}

impl WatchPlan {
    fn build(
        root: &Path,
        home: Option<&Path>,
        max_dirs: usize,
        max_entries: usize,
    ) -> Result<Self> {
        if max_dirs == 0
            || root.parent().is_none()
            || home == Some(root)
            || root.file_name().is_some_and(watch_ignored)
        {
            return Err(IndexError::UnsafeWorkspace("watch root excluded".into()));
        }
        checked_directory(root)?;
        let mut plan = Self {
            directories: vec![root.to_path_buf()],
            entries: 0,
        };
        let mut cursor = 0;
        while cursor < plan.directories.len() {
            let directory = &plan.directories[cursor];
            checked_directory(directory)?;
            // Stream entries: sorting/collecting a huge directory would defeat
            // the entry budget before the first watch was installed.
            for entry in std::fs::read_dir(directory)? {
                if plan.entries == max_entries {
                    return Err(IndexError::DiscoveryLimit("watch plan entries"));
                }
                plan.entries += 1;
                let entry = entry?;
                if watch_ignored(&entry.file_name()) {
                    continue;
                }
                // DirEntry::file_type does not follow symlinks.
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                if plan.directories.len() == max_dirs {
                    return Err(IndexError::DiscoveryLimit("workspace watches"));
                }
                plan.directories.push(entry.path());
            }
            cursor += 1;
        }
        Ok(plan)
    }

    fn install(
        &self,
        mut watch: impl FnMut(&Path, RecursiveMode) -> notify::Result<()>,
    ) -> (usize, Option<String>) {
        for (registered, directory) in self.directories.iter().enumerate() {
            let result = checked_directory(directory).and_then(|()| {
                watch(directory, RecursiveMode::NonRecursive)
                    .map_err(|error| IndexError::UnsafeWorkspace(format!("watch failed: {error}")))
            });
            if let Err(error) = result {
                return (registered, Some(error.to_string()));
            }
        }
        (self.directories.len(), None)
    }
}

// On Linux notify's Drop only queues shutdown. Keep the reservation in the
// event handler, which the backend releases after closing its inotify fd, so
// overlapping replacements cannot reuse budget while old watches still exist.
fn budgeted_watcher(
    reservation: WatchReservation<'static>,
    mut handler: impl FnMut(notify::Result<Event>) + Send + 'static,
) -> notify::Result<RecommendedWatcher> {
    notify::recommended_watcher(move |event| {
        let _keep_reservation = &reservation;
        handler(event);
    })
}

pub struct IndexWatcher {
    _watcher: Option<RecommendedWatcher>,
}

impl IndexWatcher {
    pub fn new(workspace: &RegisteredWorkspace, handle: IndexHandle) -> Result<Self> {
        let id = workspace.workspace_id;
        let root = workspace.root.clone();
        let error_handle = handle.clone();
        let home =
            std::env::var_os("HOME").and_then(|path| PathBuf::from(path).canonicalize().ok());
        let plan = match WatchPlan::build(
            &root,
            home.as_deref(),
            MAX_WORKSPACE_WATCHES,
            MAX_PLAN_ENTRIES,
        ) {
            Ok(plan) => plan,
            Err(error) => return Ok(Self::degraded(workspace, &handle, 0, 0, &error.to_string())),
        };
        let planned = plan.directories.len();
        let Some(reservation) =
            WatchReservation::acquire(&RESERVED_WATCHES, planned, MAX_TOTAL_WATCHES)
        else {
            return Ok(Self::degraded(
                workspace,
                &handle,
                planned,
                0,
                "aggregate watch budget exhausted",
            ));
        };
        let mut watcher = match budgeted_watcher(reservation, move |event| {
            route_event(&handle, id, &root, event);
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                return Ok(Self::degraded(
                    workspace,
                    &error_handle,
                    planned,
                    0,
                    &error.to_string(),
                ));
            }
        };
        let (registered, error) = plan.install(|path, mode| watcher.watch(path, mode));
        if let Some(error) = error {
            Self::degraded(workspace, &error_handle, planned, registered, &error);
        } else {
            info!(workspace_id = %id, planned, registered, entries = plan.entries,
                "Codegraph bounded watches installed; new directories use periodic reconciliation");
        }
        Ok(Self {
            _watcher: Some(watcher),
        })
    }

    fn degraded(
        workspace: &RegisteredWorkspace,
        handle: &IndexHandle,
        planned: usize,
        registered: usize,
        reason: &str,
    ) -> Self {
        warn!(workspace_id = %workspace.workspace_id, planned, registered, reason,
            "Codegraph watches degraded; periodic reconciliation remains active");
        let _ = handle.force_rescan(workspace.workspace_id);
        // Keep an inert watcher in the runtime map, so its registry refresh
        // cannot repeatedly retry an exhausted budget or unavailable backend.
        Self { _watcher: None }
    }
}

pub(super) fn route_event(
    handle: &IndexHandle,
    workspace_id: Uuid,
    root: &std::path::Path,
    event: notify::Result<Event>,
) {
    match event {
        Ok(event) if relevant(&event, root) => {
            let _ = handle.request(workspace_id);
        }
        Ok(_) => {}
        Err(_) => {
            let _ = handle.force_rescan(workspace_id);
        }
    }
}

fn relevant(event: &Event, root: &std::path::Path) -> bool {
    if !matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Remove(_)
            | EventKind::Any
            | EventKind::Other
    ) {
        return false;
    }
    if event.paths.is_empty() {
        return true;
    }
    event.paths.iter().any(|path| {
        let Ok(relative) = path.strip_prefix(root) else { return true; };
        if relative.components().any(|component| matches!(component, std::path::Component::Normal(name) if IGNORED_DIRS.iter().any(|ignored| name == *ignored))) {
            return false;
        }
        // Paths can be deleted by the time a callback runs; extension and
        // directory hints are enough to request a full scan.
        relative.extension().and_then(|ext| ext.to_str()).is_none_or(|ext| matches!(ext.to_ascii_lowercase().as_str(), "rs" | "md" | "markdown" | "toml" | "lock"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(root: &Path, max_dirs: usize, max_entries: usize) -> Result<WatchPlan> {
        WatchPlan::build(root, None, max_dirs, max_entries)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn nested_ignored_subtrees_are_pruned_before_registration() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        for ignored in IGNORED_DIRS.iter().chain(CACHE_DIRS) {
            std::fs::create_dir_all(root.join("src").join(ignored).join("deep/child")).unwrap();
        }
        std::fs::write(root.join("src/lib.rs"), "fn main() {}").unwrap();
        // Exactly root + src + nested, despite every ignored name occurring
        // below an ordinary source directory.
        let plan = plan(&root, 3, 32).unwrap();
        let mut installed = Vec::new();
        let (registered, error) = plan.install(|path, mode| {
            assert!(matches!(mode, RecursiveMode::NonRecursive));
            installed.push(path.to_path_buf());
            Ok(())
        });
        assert_eq!(error, None);
        assert_eq!(plan.directories.len(), 3);
        assert_eq!(registered, 3);
        assert_eq!(
            installed,
            vec![root.clone(), root.join("src"), root.join("src/nested")]
        );
        assert_eq!(plan.entries, 3 + IGNORED_DIRS.len() + CACHE_DIRS.len());
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn symlink_directories_and_cycles_are_pruned() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::create_dir_all(outside.path().join("deep/child")).unwrap();
        symlink(outside.path(), root.join("external")).unwrap();
        symlink(&root, root.join("src/cycle")).unwrap();
        let plan = plan(&root, 2, 3).unwrap();
        assert_eq!(plan.directories, vec![root.clone(), root.join("src")]);
        assert_eq!(plan.entries, 3);
        assert_eq!(plan.install(|_, _| Ok(())), (2, None));
        assert!(matches!(
            super::WatchPlan::build(&root.join("external"), None, 2, 3),
            Err(IndexError::UnsafeWorkspace(_))
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn planning_budget_falls_back_before_any_backend_calls() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        let mut calls = 0;
        let result = plan(&root, 2, 32).map(|plan| {
            plan.install(|_, _| {
                calls += 1;
                Ok(())
            })
        });
        assert!(matches!(
            result,
            Err(IndexError::DiscoveryLimit("workspace watches"))
        ));
        assert_eq!(calls, 0);
        // File-heavy directories are bounded too, independent of watch count.
        std::fs::write(root.join("one.rs"), "").unwrap();
        std::fs::write(root.join("two.rs"), "").unwrap();
        assert!(matches!(
            plan(&root, 8, 2),
            Err(IndexError::DiscoveryLimit("watch plan entries"))
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn aggregate_reservations_bound_overlapping_watchers_and_release_on_drop() {
        let used = AtomicUsize::new(0);
        let first = WatchReservation::acquire(&used, 3, 4).unwrap();
        assert!(WatchReservation::acquire(&used, 2, 4).is_none());
        let second = WatchReservation::acquire(&used, 1, 4).unwrap();
        assert_eq!(used.load(Ordering::Acquire), 4);
        drop(first);
        assert_eq!(used.load(Ordering::Acquire), 1);
        let replacement = WatchReservation::acquire(&used, 3, 4).unwrap();
        drop(second);
        drop(replacement);
        assert_eq!(used.load(Ordering::Acquire), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn home_and_cache_roots_use_periodic_reconciliation() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        assert!(matches!(
            WatchPlan::build(&root, Some(&root), 8, 32),
            Err(IndexError::UnsafeWorkspace(_))
        ));
        std::fs::create_dir(root.join(".cache")).unwrap();
        assert!(matches!(
            plan(&root.join(".cache"), 8, 32),
            Err(IndexError::UnsafeWorkspace(_))
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn failed_registration_stops_and_reports_exact_count() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        let plan = plan(&root, 3, 32).unwrap();
        let mut attempts = 0;
        let (registered, error) = plan.install(|_, mode| {
            assert!(matches!(mode, RecursiveMode::NonRecursive));
            attempts += 1;
            if attempts == 2 {
                Err(notify::Error::generic("watch limit"))
            } else {
                Ok(())
            }
        });
        assert_eq!(plan.directories.len(), 3);
        assert_eq!(attempts, 2);
        assert_eq!(registered, 1);
        assert!(error.unwrap().contains("watch limit"));
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn changed_directory_is_checked_again_before_installation() {
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        let plan = plan(&root, 2, 32).unwrap();
        std::fs::remove_dir(root.join("src")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("src")).unwrap();
        let mut installed = Vec::new();
        let (registered, error) = plan.install(|path, _| {
            installed.push(path.to_path_buf());
            Ok(())
        });
        assert_eq!(installed, vec![root]);
        assert_eq!(registered, 1);
        assert!(error.unwrap().contains("watch directory changed"));
    }

    // Observe kernel registrations, not the planner's own list. Match only
    // fixture inodes so unrelated concurrent tests do not affect the counts.
    #[cfg(target_os = "linux")]
    fn kernel_watches(paths: &[PathBuf]) -> Vec<usize> {
        use std::os::unix::fs::MetadataExt;
        let inodes: Vec<_> = paths
            .iter()
            .map(|path| std::fs::metadata(path).unwrap().ino())
            .collect();
        let mut counts = vec![0; paths.len()];
        for entry in std::fs::read_dir("/proc/self/fdinfo").unwrap() {
            let text = match std::fs::read_to_string(entry.unwrap().path()) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => panic!("read fdinfo: {error}"),
            };
            for line in text.lines().filter(|line| line.starts_with("inotify wd:")) {
                let inode = line
                    .split_whitespace()
                    .find_map(|field| field.strip_prefix("ino:"))
                    .map(|value| u64::from_str_radix(value, 16).unwrap())
                    .unwrap();
                for (index, expected) in inodes.iter().enumerate() {
                    if inode == *expected {
                        counts[index] += 1;
                    }
                }
            }
        }
        counts
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn real_backend_registers_only_source_directories_and_drops_kernel_watches() {
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let mut paths = vec![root.clone(), root.join("src"), root.join("src/nested")];
        std::fs::create_dir_all(&paths[2]).unwrap();
        for ignored in IGNORED_DIRS.iter().chain(CACHE_DIRS) {
            let path = root.join("src").join(ignored).join("deep/child");
            std::fs::create_dir_all(&path).unwrap();
            paths.push(path);
        }
        paths.push(outside.path().to_path_buf());
        std::os::unix::fs::symlink(outside.path(), root.join("external")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("src/cycle")).unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), &root).unwrap();
        let (_manager, handle) =
            super::super::IndexManager::new(indexes.path().to_path_buf(), vec![workspace.clone()])
                .unwrap();
        let watcher = IndexWatcher::new(&workspace, handle).unwrap();
        let mut expected = vec![0; paths.len()];
        expected[..3].fill(1);
        assert_eq!(kernel_watches(&paths), expected);
        drop(watcher);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while kernel_watches(&paths).iter().any(|count| *count != 0) {
            assert!(
                std::time::Instant::now() < deadline,
                "kernel watches survived drop"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn blocked_backend_retains_budget_until_kernel_shutdown() {
        use std::sync::mpsc;
        use std::time::Duration;
        static USED: AtomicUsize = AtomicUsize::new(0);
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut release_rx = Some(release_rx);
        let reservation = WatchReservation::acquire(&USED, 1, 1).unwrap();
        let mut watcher = budgeted_watcher(reservation, move |_| {
            if let Some(release_rx) = release_rx.take() {
                let _ = entered_tx.send(());
                let _ = release_rx.recv_timeout(Duration::from_secs(10));
            }
        })
        .unwrap();
        watcher.watch(&root, RecursiveMode::NonRecursive).unwrap();
        std::fs::write(root.join("lib.rs"), "pub fn changed() {}\n").unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(watcher); // Shutdown is queued behind the blocked callback.
        assert_eq!(kernel_watches(std::slice::from_ref(&root)), vec![1]);
        assert!(WatchReservation::acquire(&USED, 1, 1).is_none());
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while USED.load(Ordering::Acquire) != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "budget survived shutdown"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(kernel_watches(&[root]), vec![0]);
        assert!(WatchReservation::acquire(&USED, 1, 1).is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn over_budget_workspace_still_indexes_changed_and_new_source_without_watches() {
        use super::super::{IndexManager, worker};
        use std::sync::atomic::AtomicU64;
        let fixture = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        for index in 0..MAX_WORKSPACE_WATCHES {
            std::fs::create_dir(root.join(format!("dir{index}"))).unwrap();
        }
        std::fs::write(root.join("lib.rs"), "pub fn initial() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), &root).unwrap();
        let (_manager, handle) =
            IndexManager::new(indexes.path().to_path_buf(), vec![workspace.clone()]).unwrap();
        let watcher = IndexWatcher::new(&workspace, handle).unwrap();
        assert!(watcher._watcher.is_none());
        #[cfg(target_os = "linux")]
        assert_eq!(kernel_watches(std::slice::from_ref(&root)), vec![0]);
        let db = worker::project_db_path(indexes.path(), workspace.project_id);
        let mut state = worker::WorkerState::default();
        let revision = AtomicU64::new(0);
        // Exercise the same exact-content worker invoked by periodic scans.
        let first = worker::index_once(&mut state, &workspace, &db, &revision, 0).unwrap();
        let changed = b"pub fn changed() {}\n";
        let added = b"pub fn added() {}\n";
        std::fs::write(root.join("lib.rs"), changed).unwrap();
        std::fs::create_dir(root.join("new")).unwrap();
        std::fs::write(root.join("new/added.rs"), added).unwrap();
        let second = worker::index_once(&mut state, &workspace, &db, &revision, 0).unwrap();
        assert_ne!(first.ready.snapshot_digest, second.ready.snapshot_digest);
        let connection = rusqlite::Connection::open(&db).unwrap();
        for (path, bytes) in [
            ("lib.rs", changed.as_slice()),
            ("new/added.rs", added.as_slice()),
        ] {
            let digest: String = connection
                .query_row(
                    "SELECT f.source_digest FROM cg_files f JOIN cg_workspace_heads h \
                 ON h.workspace_id=f.workspace_id AND h.generation=f.generation \
                 WHERE f.workspace_id=?1 AND f.path=?2",
                    rusqlite::params![workspace.workspace_id.to_string(), path],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(digest, blake3::hash(bytes).to_hex().to_string());
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn relevant_changes_are_hints_for_full_scan() {
        let root = std::path::Path::new("/work");
        let event = Event::new(EventKind::Any).add_path(root.join("src/lib.rs"));
        assert!(relevant(&event, root));
        let ignored = Event::new(EventKind::Any).add_path(root.join("target/generated.rs"));
        assert!(!relevant(&ignored, root));
    }
}
