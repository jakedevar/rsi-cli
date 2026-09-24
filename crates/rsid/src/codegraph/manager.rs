//! Bounded, fair workspace scheduler. A single blocking writer lane serializes
//! all project databases; coalesced revisions retain one successor per active
//! workspace without allocating an unbounded task or event queue.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::bus::{DaemonEvent, EventBus};

use super::{
    IndexError, RegisteredWorkspace, Result,
    status::{IndexPhase, IndexStatus},
    worker::{self, WorkerState},
};

const INGRESS_CAPACITY: usize = 128;
const MAX_REGISTERED_WORKSPACES: usize = 128;
const RESCAN_TICK: Duration = Duration::from_secs(1);
const FULL_RECONCILE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DEBOUNCE: Duration = Duration::from_millis(150);

struct Slot {
    workspace: RegisteredWorkspace,
    revision: AtomicU64,
    rescan: AtomicBool,
    overflow: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexRequestError {
    UnknownWorkspace,
    Closed,
}

/// A workspace resolved from the manager's trusted registration snapshot.
/// S4 can use its project-bound database path without resolving a caller path.
#[derive(Debug, Clone)]
pub struct IndexWorkspaceBinding {
    pub workspace: RegisteredWorkspace,
    pub db_path: PathBuf,
}

#[derive(Clone)]
pub struct IndexHandle {
    index_root: PathBuf,
    tx: mpsc::Sender<Uuid>,
    slots: Arc<Mutex<HashMap<Uuid, Arc<Slot>>>>,
    statuses: Arc<Mutex<HashMap<Uuid, IndexStatus>>>,
    enabled: Arc<AtomicBool>,
}

impl IndexHandle {
    /// Change admission immediately. Enabling wakes a full reconciliation of
    /// every current registration; disabling invalidates an active worker.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
        if let Ok(slots) = self.slots.lock() {
            for (id, slot) in slots.iter() {
                slot.rescan.store(true, Ordering::Release);
                if !enabled {
                    slot.revision.fetch_add(1, Ordering::AcqRel);
                } else {
                    let _ = self.tx.try_send(*id);
                }
            }
        }
    }

    /// Nonblocking ingress. Full channels retain a one-bit full rescan marker
    /// and revision counter for the known workspace.
    ///
    /// # Errors
    /// Returns an error for an unknown workspace or closed manager channel.
    pub fn request(&self, workspace_id: Uuid) -> std::result::Result<(), IndexRequestError> {
        let slot = self
            .slots
            .lock()
            .map_err(|_| IndexRequestError::Closed)?
            .get(&workspace_id)
            .cloned()
            .ok_or(IndexRequestError::UnknownWorkspace)?;
        slot.revision.fetch_add(1, Ordering::AcqRel);
        match self.tx.try_send(workspace_id) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                slot.rescan.store(true, Ordering::Release);
                slot.overflow.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(IndexRequestError::Closed),
        }
    }

    /// A watcher error or lost watch must force a complete scan even if the
    /// raw event channel is full.
    ///
    /// # Errors
    /// Returns an error for an unknown workspace or closed manager channel.
    pub fn force_rescan(&self, workspace_id: Uuid) -> std::result::Result<(), IndexRequestError> {
        let slot = self
            .slots
            .lock()
            .map_err(|_| IndexRequestError::Closed)?
            .get(&workspace_id)
            .cloned()
            .ok_or(IndexRequestError::UnknownWorkspace)?;
        slot.rescan.store(true, Ordering::Release);
        self.request(workspace_id)
    }

    #[must_use]
    pub fn status(&self, workspace_id: Uuid) -> Option<IndexStatus> {
        self.statuses.lock().ok()?.get(&workspace_id).cloned()
    }

    /// Resolve only a currently registered workspace to its project database.
    ///
    /// # Errors
    /// Returns an error if the registry lock is poisoned.
    pub fn registered_workspace(
        &self,
        workspace_id: Uuid,
    ) -> Result<Option<IndexWorkspaceBinding>> {
        let slots = self
            .slots
            .lock()
            .map_err(|_| IndexError::UnsafeWorkspace("codegraph registry lock poisoned".into()))?;
        Ok(slots.get(&workspace_id).map(|slot| IndexWorkspaceBinding {
            db_path: worker::project_db_path(&self.index_root, slot.workspace.project_id),
            workspace: slot.workspace.clone(),
        }))
    }

    /// List a bounded, stable-order registration snapshot for one project.
    ///
    /// # Errors
    /// Returns an error if the registry lock is poisoned.
    pub fn registered_project_workspaces(
        &self,
        project_id: Uuid,
    ) -> Result<Vec<IndexWorkspaceBinding>> {
        let slots = self
            .slots
            .lock()
            .map_err(|_| IndexError::UnsafeWorkspace("codegraph registry lock poisoned".into()))?;
        let mut bound = slots
            .values()
            .filter(|slot| slot.workspace.project_id == project_id)
            .map(|slot| IndexWorkspaceBinding {
                db_path: worker::project_db_path(&self.index_root, project_id),
                workspace: slot.workspace.clone(),
            })
            .collect::<Vec<_>>();
        drop(slots);
        bound.sort_by_key(|item| item.workspace.workspace_id);
        Ok(bound)
    }

    /// Read the authoritative project-bound status for a registered workspace.
    /// A missing database or state row means no durable attempt has run yet.
    pub fn durable_status(
        &self,
        workspace_id: Uuid,
    ) -> Result<Option<rsi_codegraph::lifecycle::DurableIndexStatus>> {
        let project_id = self
            .slots
            .lock()
            .map_err(|_| IndexError::UnsafeWorkspace("codegraph registry lock poisoned".into()))?
            .get(&workspace_id)
            .map(|slot| slot.workspace.project_id);
        let Some(project_id) = project_id else {
            return Ok(None);
        };
        let db = worker::project_db_path(&self.index_root, project_id);
        if !db.is_file() {
            return Ok(None);
        }
        Ok(rsi_codegraph::CodegraphStore::open(db, project_id)?.index_status(workspace_id)?)
    }

    /// Apply a bounded trusted registration snapshot. Replaced slots invalidate
    /// any active worker before it can publish and enter the next scan queued.
    pub fn reconcile(&self, workspaces: Vec<RegisteredWorkspace>) -> Result<()> {
        if workspaces.len() > MAX_REGISTERED_WORKSPACES {
            return Err(IndexError::DiscoveryLimit("registered workspaces"));
        }
        let mut slots = self
            .slots
            .lock()
            .map_err(|_| IndexError::UnsafeWorkspace("codegraph registry lock poisoned".into()))?;
        let mut next = HashMap::with_capacity(workspaces.len());
        for workspace in workspaces {
            let id = workspace.workspace_id;
            if next.contains_key(&id) {
                return Err(IndexError::UnsafeWorkspace(
                    "duplicate workspace identity".into(),
                ));
            }
            let slot = if let Some(existing) = slots.get(&id)
                && existing.workspace.root == workspace.root
                && existing.workspace.project_root == workspace.project_root
            {
                existing.clone()
            } else {
                Arc::new(Slot {
                    workspace,
                    revision: AtomicU64::new(0),
                    rescan: AtomicBool::new(true),
                    overflow: AtomicU64::new(0),
                })
            };
            next.insert(id, slot);
        }
        for (id, old) in slots.iter() {
            if !next.get(id).is_some_and(|new| Arc::ptr_eq(old, new)) {
                old.revision.fetch_add(1, Ordering::AcqRel);
            }
        }
        let mut statuses = self
            .statuses
            .lock()
            .map_err(|_| IndexError::UnsafeWorkspace("codegraph status lock poisoned".into()))?;
        statuses.retain(|id, _| next.contains_key(id));
        for (id, slot) in &next {
            if !slots.get(id).is_some_and(|old| Arc::ptr_eq(old, slot)) {
                statuses.insert(*id, IndexStatus::new(slot.workspace.project_id, *id));
            }
        }
        *slots = next;
        Ok(())
    }
}

pub struct IndexManager {
    index_root: PathBuf,
    rx: mpsc::Receiver<Uuid>,
    slots: Arc<Mutex<HashMap<Uuid, Arc<Slot>>>>,
    statuses: Arc<Mutex<HashMap<Uuid, IndexStatus>>>,
    worker_states: HashMap<Uuid, WorkerState>,
    worker_roots: HashMap<Uuid, PathBuf>,
    pending: VecDeque<Uuid>,
    pending_set: HashSet<Uuid>,
    events: Option<Arc<EventBus>>,
    enabled: Arc<AtomicBool>,
}

impl IndexManager {
    /// All workspace registrations come from trusted daemon project and
    /// sandbox/worktree records. Duplicate identities fail closed.
    ///
    /// # Errors
    /// Returns an error if registration count or identities exceed bounds.
    pub fn new(
        index_root: PathBuf,
        workspaces: Vec<RegisteredWorkspace>,
    ) -> Result<(Self, IndexHandle)> {
        Self::new_with_enabled(index_root, workspaces, Arc::new(AtomicBool::new(false)))
    }

    /// Inject the persisted daemon setting's shared flag. The caller owns
    /// initial value; `IndexHandle::set_enabled` wakes reconciliation on ON.
    pub fn new_with_enabled(
        index_root: PathBuf,
        workspaces: Vec<RegisteredWorkspace>,
        enabled: Arc<AtomicBool>,
    ) -> Result<(Self, IndexHandle)> {
        if workspaces.len() > MAX_REGISTERED_WORKSPACES {
            return Err(IndexError::DiscoveryLimit("registered workspaces"));
        }
        let mut stores = HashMap::new();
        for project_id in workspaces.iter().map(|workspace| workspace.project_id) {
            if stores.contains_key(&project_id) {
                continue;
            }
            let db = worker::project_db_path(&index_root, project_id);
            if db.exists() {
                let mut store = rsi_codegraph::CodegraphStore::open(&db, project_id)?;
                store.recover_interrupted_index_runs()?;
                store.generation_details()?;
                stores.insert(project_id, store);
            }
        }
        let mut slots = HashMap::new();
        let mut statuses = HashMap::new();
        for workspace in workspaces {
            let id = workspace.workspace_id;
            if slots.contains_key(&id) {
                return Err(IndexError::UnsafeWorkspace(
                    "duplicate workspace identity".into(),
                ));
            }
            let durable = stores
                .get(&workspace.project_id)
                .map(|store| store.index_status(id))
                .transpose()?
                .flatten();
            statuses.insert(
                id,
                durable.map_or_else(
                    || IndexStatus::new(workspace.project_id, id),
                    |status| IndexStatus::from_durable(workspace.project_id, status),
                ),
            );
            slots.insert(
                id,
                Arc::new(Slot {
                    workspace,
                    revision: AtomicU64::new(0),
                    rescan: AtomicBool::new(true),
                    overflow: AtomicU64::new(0),
                }),
            );
        }
        let slots = Arc::new(Mutex::new(slots));
        let statuses = Arc::new(Mutex::new(statuses));
        let (tx, rx) = mpsc::channel(INGRESS_CAPACITY);
        let handle = IndexHandle {
            index_root: index_root.clone(),
            tx,
            slots: slots.clone(),
            statuses: statuses.clone(),
            enabled: enabled.clone(),
        };
        let manager = Self {
            index_root,
            rx,
            slots,
            statuses,
            worker_states: HashMap::new(),
            worker_roots: HashMap::new(),
            pending: VecDeque::new(),
            pending_set: HashSet::new(),
            events: None,
            enabled,
        };
        Ok((manager, handle))
    }

    pub fn with_event_bus(mut self, events: Arc<EventBus>) -> Self {
        self.events = Some(events);
        self
    }

    fn queue(&mut self, id: Uuid) {
        if self.slots.lock().is_ok_and(|slots| slots.contains_key(&id))
            && self.pending_set.insert(id)
        {
            self.pending.push_back(id);
        }
    }

    fn queue_rescans(&mut self) {
        let ids = self.slots.lock().map_or_else(
            |_| Vec::new(),
            |slots| {
                slots
                    .iter()
                    .filter_map(|(id, slot)| {
                        slot.rescan.swap(false, Ordering::AcqRel).then_some(*id)
                    })
                    .collect::<Vec<_>>()
            },
        );
        for id in ids {
            self.queue(id);
        }
    }

    fn update_status(&self, id: Uuid, update: impl FnOnce(&mut IndexStatus)) {
        if let Ok(mut statuses) = self.statuses.lock()
            && let Some(status) = statuses.get_mut(&id)
        {
            update(status);
        }
    }

    /// Drive startup reconciliation and periodic exact-byte rescans. The
    /// manager owns the only writer lane, regardless of workspace count.
    pub async fn run(self) {
        self.run_with_intervals(RESCAN_TICK, FULL_RECONCILE_INTERVAL)
            .await;
    }

    #[allow(clippy::too_many_lines)] // Scheduling and worker settlement share one bounded queue.
    async fn run_with_intervals(mut self, rescan_tick: Duration, full_reconcile: Duration) {
        if let Some(events) = &self.events
            && let Ok(statuses) = self.statuses.lock()
        {
            for status in statuses
                .values()
                .filter(|status| status.phase == IndexPhase::Recovering)
            {
                events.publish(DaemonEvent::CodegraphIndexStatus {
                    project_id: status.project_id,
                    workspace_id: status.workspace_id,
                    phase: rsi_codegraph::lifecycle::IndexLifecyclePhase::Recovering,
                    generation: status.ready.as_ref().map(|ready| ready.generation),
                    run_id: None,
                });
            }
        }
        let mut was_enabled = false;
        let mut tick = tokio::time::interval(rescan_tick);
        let mut last_full_scan = Instant::now();
        loop {
            if !self.enabled.load(Ordering::Acquire) {
                was_enabled = false;
                self.pending.clear();
                self.pending_set.clear();
                tokio::select! {
                    value = self.rx.recv() => if value.is_none() { break; },
                    _ = tick.tick() => {},
                }
                continue;
            }
            if !was_enabled {
                if let Ok(slots) = self.slots.lock() {
                    for slot in slots.values() {
                        slot.rescan.store(true, Ordering::Release);
                    }
                }
                was_enabled = true;
            }
            if self.pending.is_empty() {
                tokio::select! {
                    value = self.rx.recv() => match value {
                        Some(id) => self.queue(id),
                        None => break,
                    },
                    _ = tick.tick() => {},
                }
            }
            if last_full_scan.elapsed() >= full_reconcile {
                let ids = self.slots.lock().map_or_else(
                    |_| Vec::new(),
                    |slots| slots.keys().copied().collect::<Vec<_>>(),
                );
                for id in ids {
                    self.queue(id);
                }
                last_full_scan = Instant::now();
            }
            self.queue_rescans();
            // Coalesce bursts before taking a workspace. FIFO order still
            // rotates distinct workspaces fairly.
            tokio::time::sleep(DEBOUNCE).await;
            if !self.enabled.load(Ordering::Acquire) {
                continue;
            }
            while let Ok(id) = self.rx.try_recv() {
                self.queue(id);
            }
            let Some(id) = self.pending.pop_front() else {
                continue;
            };
            self.pending_set.remove(&id);
            let Some(slot) = self
                .slots
                .lock()
                .ok()
                .and_then(|slots| slots.get(&id).cloned())
            else {
                continue;
            };
            if !self.enabled.load(Ordering::Acquire) {
                slot.rescan.store(true, Ordering::Release);
                continue;
            }
            let observed_revision = slot.revision.load(Ordering::Acquire);
            let workspace = slot.workspace.clone();
            let db = worker::project_db_path(&self.index_root, workspace.project_id);
            let mut state = if self.worker_roots.get(&id) == Some(&workspace.root) {
                self.worker_states.remove(&id).unwrap_or_default()
            } else {
                WorkerState::default()
            };
            self.worker_roots.insert(id, workspace.root.clone());
            self.update_status(id, |status| {
                status.phase = IndexPhase::Building;
                status.pending_rescan = false;
            });
            let started = Instant::now();
            let slot_for_run = slot.clone();
            let events = self.events.clone();
            let enabled = self.enabled.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let result = worker::index_once_with_events_and_gate(
                    &mut state,
                    &workspace,
                    &db,
                    &slot_for_run.revision,
                    observed_revision,
                    slot_for_run.overflow.load(Ordering::Relaxed),
                    events.as_deref(),
                    &enabled,
                );
                let ready_on_error = if result.is_err() && db.exists() {
                    rsi_codegraph::CodegraphStore::open(&db, workspace.project_id)
                        .and_then(|store| store.current_ready(workspace.workspace_id))
                        .ok()
                } else {
                    None
                };
                (state, result, ready_on_error)
            })
            .await;
            let (result, ready_on_error) = match outcome {
                Ok((state, result, ready_on_error)) => {
                    self.worker_states.insert(id, state);
                    (result, ready_on_error)
                }
                Err(error) => (
                    Err(IndexError::UnsafeWorkspace(format!(
                        "index worker stopped: {error}"
                    ))),
                    None,
                ),
            };
            let superseded = slot.revision.load(Ordering::Acquire) != observed_revision;
            let still_registered = self.slots.lock().is_ok_and(|slots| {
                slots
                    .get(&id)
                    .is_some_and(|current| Arc::ptr_eq(current, &slot))
            });
            if !still_registered {
                self.worker_states.remove(&id);
                self.worker_roots.remove(&id);
                self.queue_rescans();
                continue;
            }
            self.update_status(id, |status| {
                status.last_duration = Some(started.elapsed());
                status.overflow_count = slot.overflow.load(Ordering::Relaxed);
                status.pending_rescan = slot.rescan.load(Ordering::Acquire) || superseded;
                match result {
                    Ok(outcome) => {
                        status.files_discovered = outcome.files_discovered;
                        status.bytes_discovered = outcome.bytes_discovered;
                        status.files_hashed = outcome.files_hashed;
                        status.files_reused = outcome.files_reused;
                        status.files_extracted = outcome.files_extracted;
                        status.changed_paths = outcome.changed_paths;
                        status.staged_files = outcome.staged_files;
                        status.last_published = outcome.published;
                        status.ready = Some(outcome.ready);
                        status.phase = if superseded {
                            IndexPhase::Stale
                        } else {
                            IndexPhase::Ready
                        };
                        status.last_error = None;
                        status.rescan_reason = superseded.then(|| "revision_changed".into());
                    }
                    Err(error) => {
                        if status.ready.is_none() {
                            status.ready = ready_on_error;
                        }
                        status.phase = if status.ready.is_some() {
                            IndexPhase::Degraded
                        } else {
                            IndexPhase::Failed
                        };
                        status.last_error = Some(error.to_string());
                        status.rescan_reason = Some("run_failed".into());
                    }
                }
            });
            if superseded {
                self.queue(id);
            }
            let registered = self.slots.lock().map_or_else(
                |_| HashSet::new(),
                |slots| slots.keys().copied().collect::<HashSet<_>>(),
            );
            self.worker_states.retain(|id, _| registered.contains(id));
            self.worker_roots.retain(|id, _| registered.contains(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_for_ready_file(
        handle: &IndexHandle,
        workspace: &RegisteredWorkspace,
        db: &std::path::Path,
        expected: &[u8],
    ) {
        let expected_digest = blake3::hash(expected).to_hex().to_string();
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if handle.status(workspace.workspace_id).is_some_and(|status| {
                    status.phase == IndexPhase::Ready && !status.pending_rescan
                }) && let Ok(connection) = rusqlite::Connection::open(db)
                {
                    let digest = connection.query_row(
                        "SELECT f.source_digest FROM cg_files f JOIN cg_workspace_heads h \
                         ON h.workspace_id = f.workspace_id AND h.generation = f.generation \
                         WHERE f.workspace_id = ?1 AND f.path = 'lib.rs'",
                        [workspace.workspace_id.to_string()],
                        |row| row.get::<_, String>(0),
                    );
                    if digest.is_ok_and(|digest| digest == expected_digest) {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        if result.is_err() {
            let files = (|| -> rusqlite::Result<Vec<(String, String)>> {
                let connection = rusqlite::Connection::open(db)?;
                let mut statement = connection.prepare(
                    "SELECT path, source_digest FROM cg_files ORDER BY generation DESC LIMIT 5",
                )?;
                statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect()
            })();
            panic!(
                "ready generation did not converge to {:?}: status={:?}, files={files:?}",
                String::from_utf8_lossy(expected),
                handle.status(workspace.workspace_id)
            );
        }
    }

    #[tokio::test]
    async fn default_off_blocks_start_and_requests_then_on_reconciles_without_restart() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn first() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        let db = worker::project_db_path(indexes.path(), workspace.project_id);
        let (manager, handle) =
            IndexManager::new(indexes.path().to_path_buf(), vec![workspace.clone()]).unwrap();
        let task = tokio::spawn(manager.run());
        handle.request(workspace.workspace_id).unwrap();
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(!db.exists());

        handle.set_enabled(true);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if handle
                    .status(workspace.workspace_id)
                    .is_some_and(|status| status.phase == IndexPhase::Ready)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let ready = rsi_codegraph::CodegraphStore::open(&db, workspace.project_id)
            .unwrap()
            .current_ready(workspace.workspace_id)
            .unwrap();
        handle.set_enabled(false);
        std::fs::write(root.path().join("lib.rs"), "pub fn second() {}\n").unwrap();
        handle.request(workspace.workspace_id).unwrap();
        tokio::time::sleep(Duration::from_millis(350)).await;
        let retained = rsi_codegraph::CodegraphStore::open(&db, workspace.project_id)
            .unwrap()
            .current_ready(workspace.workspace_id)
            .unwrap();
        assert_eq!(retained, ready);

        handle.set_enabled(true);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let current = rsi_codegraph::CodegraphStore::open(&db, workspace.project_id)
                    .unwrap()
                    .current_ready(workspace.workspace_id)
                    .unwrap();
                if current != ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
    }

    #[test]
    fn startup_recovers_interrupted_run_and_retains_ready_head() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn ready() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        let db = worker::project_db_path(indexes.path(), workspace.project_id);
        let ready = worker::index_once(
            &mut WorkerState::default(),
            &workspace,
            &db,
            &AtomicU64::new(0),
            0,
        )
        .unwrap()
        .ready;
        let mut store = rsi_codegraph::CodegraphStore::open(&db, workspace.project_id).unwrap();
        store
            .begin_index_run(
                workspace.workspace_id,
                &"a".repeat(64),
                &worker::extraction_manifest(),
            )
            .unwrap();
        drop(store);
        let (_manager, handle) =
            IndexManager::new(indexes.path().to_path_buf(), vec![workspace.clone()]).unwrap();
        let status = handle.status(workspace.workspace_id).unwrap();
        assert_eq!(status.phase, IndexPhase::Recovering);
        assert!(status.pending_rescan);
        assert_eq!(status.ready.unwrap().snapshot_digest, ready.snapshot_digest);
        let durable = handle
            .durable_status(workspace.workspace_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            durable.phase,
            rsi_codegraph::lifecycle::IndexLifecyclePhase::Recovering
        );
        assert_eq!(
            durable.ready.unwrap().snapshot_digest,
            ready.snapshot_digest
        );
    }

    #[test]
    fn startup_repairs_ready_head_without_v5_accounting() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn ready() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        let db = worker::project_db_path(indexes.path(), workspace.project_id);
        let ready = worker::index_once(
            &mut WorkerState::default(),
            &workspace,
            &db,
            &AtomicU64::new(0),
            0,
        )
        .unwrap()
        .ready;
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute(
                "DELETE FROM cg_generation_detail WHERE workspace_id=?1 AND generation=?2",
                rusqlite::params![workspace.workspace_id.to_string(), ready.generation],
            )
            .unwrap();
        connection
            .execute(
                "DELETE FROM cg_retained_nodes WHERE workspace_id=?1",
                [workspace.workspace_id.to_string()],
            )
            .unwrap();
        drop(connection);

        IndexManager::new(indexes.path().to_path_buf(), vec![workspace.clone()]).unwrap();
        let mut store = rsi_codegraph::CodegraphStore::open(&db, workspace.project_id).unwrap();
        let details = store.generation_details().unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].generation, ready.generation);
        assert!(details[0].current);
        assert!(details[0].detailed_bytes > 0);
        let retained_count: i64 = rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM cg_retained_nodes WHERE workspace_id=?1",
                [workspace.workspace_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(retained_count > 0);
    }

    #[test]
    fn bounded_ingress_retains_rescan_on_overflow() {
        let root = tempfile::tempdir().unwrap();
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        let id = workspace.workspace_id;
        let (_manager, handle) =
            IndexManager::new(root.path().join("index"), vec![workspace]).unwrap();
        for _ in 0..(INGRESS_CAPACITY + 5) {
            handle.request(id).unwrap();
        }
        let slots = handle.slots.lock().unwrap();
        let slot = slots.get(&id).unwrap();
        assert!(slot.rescan.load(Ordering::Acquire));
        assert_eq!(slot.overflow.load(Ordering::Relaxed), 5);
        assert_eq!(
            slot.revision.load(Ordering::Acquire),
            (INGRESS_CAPACITY + 5) as u64
        );
    }

    #[tokio::test]
    async fn lost_reordered_and_overflowed_watcher_hints_converge() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        let path = root.path().join("lib.rs");
        let workspace = RegisteredWorkspace::primary(Uuid::new_v4(), root.path()).unwrap();
        let db = worker::project_db_path(indexes.path(), workspace.project_id);
        std::fs::write(&path, b"pub fn first() {}\n").unwrap();
        let (manager, handle) =
            IndexManager::new(indexes.path().to_path_buf(), vec![workspace.clone()]).unwrap();
        handle.set_enabled(true);
        let task = tokio::spawn(
            manager.run_with_intervals(Duration::from_millis(20), Duration::from_millis(250)),
        );
        wait_for_ready_file(&handle, &workspace, &db, b"pub fn first() {}\n").await;

        // No callback at all: the periodic content scan must find changed bytes.
        std::fs::write(&path, b"pub fn second() {}\n").unwrap();
        wait_for_ready_file(&handle, &workspace, &db, b"pub fn second() {}\n").await;

        // Deliver old and duplicate hints after a newer file state exists.
        let hint = || notify::Event::new(notify::EventKind::Any).add_path(path.clone());
        std::fs::write(&path, b"pub fn third() {}\n").unwrap();
        super::super::watcher::route_event(
            &handle,
            workspace.workspace_id,
            root.path(),
            Ok(hint()),
        );
        std::fs::write(&path, b"pub fn fourth() {}\n").unwrap();
        for _ in 0..3 {
            super::super::watcher::route_event(
                &handle,
                workspace.workspace_id,
                root.path(),
                Ok(hint()),
            );
        }
        wait_for_ready_file(&handle, &workspace, &db, b"pub fn fourth() {}\n").await;

        // The bounded callback channel overflows; its rescan marker still wins.
        std::fs::write(&path, b"pub fn fifth() {}\n").unwrap();
        for _ in 0..(INGRESS_CAPACITY + 5) {
            super::super::watcher::route_event(
                &handle,
                workspace.workspace_id,
                root.path(),
                Ok(hint()),
            );
        }
        let overflow = handle
            .slots
            .lock()
            .unwrap()
            .get(&workspace.workspace_id)
            .unwrap()
            .overflow
            .load(Ordering::Relaxed);
        assert!(overflow > 0);
        wait_for_ready_file(&handle, &workspace, &db, b"pub fn fifth() {}\n").await;
        assert!(
            handle
                .status(workspace.workspace_id)
                .unwrap()
                .overflow_count
                > 0
        );
        task.abort();
    }

    #[test]
    fn queue_coalesces_and_rotates_workspaces() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let first = RegisteredWorkspace::primary(Uuid::new_v4(), a.path()).unwrap();
        let second = RegisteredWorkspace::primary(Uuid::new_v4(), b.path()).unwrap();
        let (mut manager, _) =
            IndexManager::new(a.path().join("index"), vec![first.clone(), second.clone()]).unwrap();
        manager.queue(first.workspace_id);
        manager.queue(first.workspace_id);
        manager.queue(second.workspace_id);
        assert_eq!(manager.pending.len(), 2);
        assert_eq!(manager.pending.pop_front(), Some(first.workspace_id));
        assert_eq!(manager.pending.pop_front(), Some(second.workspace_id));
    }

    #[tokio::test]
    async fn startup_reconciles_two_workspaces_in_one_project_db() {
        let primary_root = tempfile::tempdir().unwrap();
        let external_root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::create_dir(primary_root.path().join(".git")).unwrap();
        std::fs::write(
            external_root.path().join(".git"),
            format!("gitdir: {}\n", primary_root.path().join(".git").display()),
        )
        .unwrap();
        std::fs::write(primary_root.path().join("lib.rs"), "pub fn primary() {}\n").unwrap();
        std::fs::write(
            external_root.path().join("lib.rs"),
            "pub fn external() {}\n",
        )
        .unwrap();
        let project = Uuid::new_v4();
        let primary = RegisteredWorkspace::primary(project, primary_root.path()).unwrap();
        let external = RegisteredWorkspace::registered_checkout(
            project,
            primary_root.path(),
            external_root.path(),
            rsi_codegraph::WorkspaceInstanceKey::GitWorktree(Uuid::new_v4()),
        )
        .unwrap();
        let (manager, handle) = IndexManager::new(
            indexes.path().to_path_buf(),
            vec![primary.clone(), external.clone()],
        )
        .unwrap();
        handle.set_enabled(true);
        let task = tokio::spawn(manager.run());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let a = handle.status(primary.workspace_id).unwrap();
                let b = handle.status(external.workspace_id).unwrap();
                if a.phase == IndexPhase::Ready && b.phase == IndexPhase::Ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let db = worker::project_db_path(indexes.path(), project);
        let store = rsi_codegraph::CodegraphStore::open(db, project).unwrap();
        let primary_ready = store.current_ready(primary.workspace_id).unwrap();
        let external_ready = store.current_ready(external.workspace_id).unwrap();
        assert_ne!(primary_ready.workspace_id, external_ready.workspace_id);
        assert_ne!(
            primary_ready.snapshot_digest,
            external_ready.snapshot_digest
        );
        drop(handle);
        task.abort();
    }
}
