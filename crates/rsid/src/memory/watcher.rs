use crate::error::Result;
use crate::memory::worker::MemoryHandle;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Directories to ignore when watching for memory file changes.
const IGNORED_DIR_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    ".pnpm-store",
    ".venv",
    "venv",
    ".tox",
    "__pycache__",
];

/// File watcher for the memory directory. Uses the `notify` crate to watch
/// for file changes, debounces events, and sends `SyncNow` commands to the
/// memory worker.
pub struct MemoryFileWatcher {
    /// The underlying notify watcher. Dropping this stops file watching.
    _watcher: RecommendedWatcher,
    /// Handle to the debounce task. Dropping this cancels debouncing.
    _debounce_task: JoinHandle<()>,
    /// Sender to signal the debounce task to stop.
    stop_tx: mpsc::Sender<()>,
}

impl MemoryFileWatcher {
    /// Create a new file watcher that watches `memory_dir` for changes.
    ///
    /// File system events are debounced for `debounce_ms` milliseconds.
    /// After the debounce window, a `SyncNow` command is sent to the
    /// memory worker via the provided `handle`.
    pub fn new(memory_dir: PathBuf, debounce_ms: u64, handle: MemoryHandle) -> Result<Self> {
        let (raw_tx, raw_rx) = mpsc::channel::<()>(16);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);

        // Create the notify watcher
        let event_tx = raw_tx.clone();
        let watch_dir = memory_dir.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<Event>| match res {
                Ok(event) => {
                    if should_process_event(&event, &watch_dir) {
                        let _ = event_tx.try_send(());
                    }
                }
                Err(e) => {
                    warn!("memory file watcher error: {e}");
                }
            })
            .map_err(|e| {
                crate::error::DaemonError::InvalidParam(format!(
                    "failed to create file watcher: {e}"
                ))
            })?;

        // Watch the memory subdirectory recursively
        if memory_dir.exists() {
            watcher
                .watch(&memory_dir, RecursiveMode::Recursive)
                .map_err(|e| {
                    crate::error::DaemonError::InvalidParam(format!(
                        "failed to watch {}: {e}",
                        memory_dir.display()
                    ))
                })?;
        } else if let Some(parent) = memory_dir.parent() {
            // If the directory doesn't exist yet, watch the parent for its creation.
            let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
        }

        // Spawn the debounce task
        let debounce_task = tokio::spawn(debounce_loop(
            raw_rx,
            stop_rx,
            handle,
            Duration::from_millis(debounce_ms),
        ));

        Ok(Self {
            _watcher: watcher,
            _debounce_task: debounce_task,
            stop_tx,
        })
    }

    /// Stop the file watcher and debounce task.
    pub fn stop(self) {
        let _ = self.stop_tx.try_send(());
    }
}

/// Check whether a notify event should trigger a sync.
fn should_process_event(event: &Event, _watch_dir: &PathBuf) -> bool {
    // Only care about file create, modify, remove events
    match event.kind {
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {}
        _ => return false,
    }

    for path in &event.paths {
        // Skip ignored directories
        let dominated_by_ignored = path.components().any(|c| {
            if let std::path::Component::Normal(name) = c {
                let name_str = name.to_string_lossy();
                IGNORED_DIR_NAMES
                    .iter()
                    .any(|ignored| name_str.eq_ignore_ascii_case(ignored))
            } else {
                false
            }
        });

        if dominated_by_ignored {
            continue;
        }

        // Only care about .md files (or directory events which might contain .md files)
        if path.is_dir() {
            return true;
        }
        if let Some(ext) = path.extension()
            && ext.eq_ignore_ascii_case("md")
        {
            return true;
        }
    }

    false
}

/// Debounce loop: waits for raw file system events, coalesces them within
/// a time window, then sends a single SyncNow command.
async fn debounce_loop(
    mut raw_rx: mpsc::Receiver<()>,
    mut stop_rx: mpsc::Receiver<()>,
    handle: MemoryHandle,
    debounce_duration: Duration,
) {
    loop {
        // Wait for the first event or stop signal
        tokio::select! {
            event = raw_rx.recv() => {
                if event.is_none() {
                    break; // channel closed
                }
            }
            _ = stop_rx.recv() => {
                break; // stop signal
            }
        }

        // First event received -- start the debounce window
        loop {
            tokio::select! {
                _ = raw_rx.recv() => {
                    // Another event arrived, reset the timer by continuing
                    continue;
                }
                _ = tokio::time::sleep(debounce_duration) => {
                    break;
                }
                _ = stop_rx.recv() => {
                    return; // stop signal
                }
            }
        }

        // Debounce window closed -- trigger sync
        debug!("memory file watcher: debounce window closed, triggering sync");
        if let Err(e) = handle.memory_files_changed().await {
            warn!("memory file watcher: failed to send MemoryFilesChanged: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::worker::MemoryCommand;
    use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};

    fn make_event(kind: EventKind, paths: Vec<PathBuf>) -> Event {
        Event {
            kind,
            paths,
            attrs: Default::default(),
        }
    }

    #[test]
    fn test_should_process_event_create_md() {
        let watch_dir = PathBuf::from("/tmp/memory");
        let event = make_event(
            EventKind::Create(CreateKind::File),
            vec![PathBuf::from("/tmp/memory/test.md")],
        );
        assert!(should_process_event(&event, &watch_dir));
    }

    #[test]
    fn test_should_process_event_modify_md() {
        let watch_dir = PathBuf::from("/tmp/memory");
        let event = make_event(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            vec![PathBuf::from("/tmp/memory/test.md")],
        );
        assert!(should_process_event(&event, &watch_dir));
    }

    #[test]
    fn test_should_process_event_remove_md() {
        let watch_dir = PathBuf::from("/tmp/memory");
        let event = make_event(
            EventKind::Remove(RemoveKind::File),
            vec![PathBuf::from("/tmp/memory/test.md")],
        );
        assert!(should_process_event(&event, &watch_dir));
    }

    #[test]
    fn test_should_process_event_non_md() {
        let watch_dir = PathBuf::from("/tmp/memory");
        let event = make_event(
            EventKind::Create(CreateKind::File),
            vec![PathBuf::from("/tmp/memory/test.txt")],
        );
        assert!(!should_process_event(&event, &watch_dir));
    }

    #[test]
    fn test_should_process_event_ignored_dir() {
        let watch_dir = PathBuf::from("/tmp/memory");
        let event = make_event(
            EventKind::Create(CreateKind::File),
            vec![PathBuf::from("/tmp/memory/.git/refs/test.md")],
        );
        assert!(!should_process_event(&event, &watch_dir));
    }

    #[test]
    fn test_should_process_event_node_modules() {
        let watch_dir = PathBuf::from("/tmp/memory");
        let event = make_event(
            EventKind::Create(CreateKind::File),
            vec![PathBuf::from("/tmp/memory/node_modules/pkg/README.md")],
        );
        assert!(!should_process_event(&event, &watch_dir));
    }

    #[test]
    fn test_should_process_event_access() {
        let watch_dir = PathBuf::from("/tmp/memory");
        let event = make_event(
            EventKind::Access(AccessKind::Read),
            vec![PathBuf::from("/tmp/memory/test.md")],
        );
        assert!(!should_process_event(&event, &watch_dir));
    }

    #[tokio::test]
    async fn test_debounce_coalesces_events() {
        let (raw_tx, raw_rx) = mpsc::channel::<()>(16);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let handle = MemoryHandle::new(cmd_tx);

        let task = tokio::spawn(debounce_loop(
            raw_rx,
            stop_rx,
            handle,
            Duration::from_millis(50),
        ));

        // Send 10 events rapidly
        for _ in 0..10 {
            raw_tx.send(()).await.unwrap();
        }

        // Wait past debounce window
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should receive exactly one file-change command.
        let cmd = cmd_rx.try_recv().unwrap();
        assert!(matches!(cmd, MemoryCommand::MemoryFilesChanged));

        // No more commands
        assert!(cmd_rx.try_recv().is_err());

        let _ = stop_tx.send(()).await;
        let _ = task.await;
    }

    #[tokio::test]
    async fn test_debounce_fires_after_quiet_period() {
        let (raw_tx, raw_rx) = mpsc::channel::<()>(16);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let handle = MemoryHandle::new(cmd_tx);

        let task = tokio::spawn(debounce_loop(
            raw_rx,
            stop_rx,
            handle,
            Duration::from_millis(50),
        ));

        // Send one event
        raw_tx.send(()).await.unwrap();

        // Wait past debounce window
        tokio::time::sleep(Duration::from_millis(150)).await;

        let cmd = cmd_rx.try_recv();
        assert!(cmd.is_ok(), "should fire after debounce period");

        let _ = stop_tx.send(()).await;
        let _ = task.await;
    }

    #[tokio::test]
    async fn test_debounce_separate_windows() {
        let (raw_tx, raw_rx) = mpsc::channel::<()>(16);
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let handle = MemoryHandle::new(cmd_tx);

        let task = tokio::spawn(debounce_loop(
            raw_rx,
            stop_rx,
            handle,
            Duration::from_millis(50),
        ));

        // First burst
        raw_tx.send(()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(cmd_rx.try_recv().is_ok(), "first window should fire");

        // Second burst
        raw_tx.send(()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(cmd_rx.try_recv().is_ok(), "second window should fire");

        let _ = stop_tx.send(()).await;
        let _ = task.await;
    }
}
