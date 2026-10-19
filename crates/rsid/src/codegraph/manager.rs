//! Bounded, fair workspace scheduler. A single blocking writer lane serializes
//! all project databases; coalesced revisions retain one successor per active
//! workspace without allocating an unbounded task or event queue.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use rsi_codegraph::WorkspaceInstanceKey;
use rsi_common::codegraph::{CODEGRAPH_UNHEALTHY_NOTICE_SECS, CodegraphHealthNoticeV1};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::bus::{DaemonEvent, EventBus};
use crate::store::codegraph_failure_notice::CodegraphFailureReport;

use super::{
    IndexError, RegisteredWorkspace, Result,
    status::{IndexPhase, IndexStatus},
    worker::{self, WorkerState},
};

const INGRESS_CAPACITY: usize = 128;
const MAX_REGISTERED_WORKSPACES: usize = 128;
// One codegraph `ExtractionCache` keeps ~340-390 MB live per rsi checkout and a
// pass peaks near 750 MB (#960 dhat probe), so only the workspace indexed last
// keeps its incremental cache; the others re-extract from their sources.
const MAX_RESIDENT_WORKER_STATES: usize = 1;
const RESCAN_TICK: Duration = Duration::from_secs(1);
const FULL_RECONCILE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DEBOUNCE: Duration = Duration::from_millis(150);

/// Identical real failures in a row before retries start backing off (#2246).
const BACKOFF_FREE_FAILURES: u32 = 3;
const BACKOFF_BASE: Duration = Duration::from_secs(60);
const BACKOFF_CAP: Duration = Duration::from_secs(6 * 60 * 60);

/// A workspace whose runs keep failing the same way is retried on an
/// exponentially growing delay instead of on every watcher event and
/// five-minute reconcile (#2246: 1,590 identical symlink-preflight failures).
#[derive(Debug, Clone)]
struct FailureBackoff {
    class: String,
    count: u32,
    until: Option<Instant>,
    /// A request arrived while backed off; queue it when the delay ends.
    deferred: bool,
}

impl FailureBackoff {
    /// Delay before retry number `count + 1` of one unchanged failure.
    fn delay(count: u32) -> Option<Duration> {
        let over = count.checked_sub(BACKOFF_FREE_FAILURES)?;
        Some(
            BACKOFF_BASE
                .saturating_mul(1u32.checked_shl(over.min(16)).unwrap_or(u32::MAX))
                .min(BACKOFF_CAP),
        )
    }

    fn record(previous: Option<&Self>, class: String, now: Instant) -> Self {
        let count = match previous {
            Some(previous) if previous.class == class => previous.count.saturating_add(1),
            _ => 1,
        };
        Self {
            class,
            count,
            until: Self::delay(count).map(|delay| now + delay),
            deferred: false,
        }
    }

    fn active(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }
}

/// The failure class a repeat is judged by, or `None` for a transient outcome
/// (a newer request, a reader holding the database, a source that moved under
/// the scan) that a retry may fix and that never backs off. Byte counts are
/// dropped so a write-budget failure repeats as one class.
fn failure_class(error: &IndexError) -> Option<String> {
    match error {
        IndexError::Superseded | IndexError::ActiveReader | IndexError::SourceChanged => None,
        IndexError::DiskBudget { .. } => Some("disk_budget".into()),
        other => Some(other.to_string()),
    }
}

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
    #[cfg(test)]
    resident_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl IndexHandle {
    #[cfg(test)]
    fn resident_worker_state_count(&self) -> usize {
        self.resident_count.load(Ordering::Relaxed)
    }

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

    #[cfg(test)]
    pub(super) fn with_status_for_test(&self, id: Uuid, update: impl FnOnce(&mut IndexStatus)) {
        update(self.statuses.lock().unwrap().get_mut(&id).unwrap());
    }

    /// The current failure episode of every registered workspace, primary or
    /// not, in workspace order (#2454). Unlike [`Self::unhealthy_notices`] it
    /// has no age threshold and covers sandbox and worktree checkouts: it is
    /// the owner's first signal, delivered before the day-old project Issue.
    #[must_use]
    pub fn failure_episodes(&self) -> Vec<CodegraphFailureReport> {
        // The two registries are never held together.
        let instances = self.slots.lock().map_or_else(
            |_| Vec::new(),
            |slots| {
                slots
                    .iter()
                    .map(|(id, slot)| (*id, slot.workspace.instance.clone()))
                    .collect::<Vec<_>>()
            },
        );
        let Ok(statuses) = self.statuses.lock() else {
            return Vec::new();
        };
        let mut episodes = instances
            .into_iter()
            .filter_map(|(id, instance)| {
                let episode = statuses.get(&id)?.failure_episode()?;
                let (checkout, sandbox_custody_id) = match instance {
                    WorkspaceInstanceKey::Primary => ("primary", None),
                    WorkspaceInstanceKey::GitWorktree(_) => ("worktree", None),
                    WorkspaceInstanceKey::RsiSandbox(custody) => ("sandbox", Some(custody)),
                    WorkspaceInstanceKey::DetachedRootHash(_) => ("detached", None),
                };
                Some(CodegraphFailureReport {
                    project_id: episode.project_id,
                    workspace_id: episode.workspace_id,
                    checkout: checkout.into(),
                    sandbox_custody_id,
                    phase: episode.phase.into(),
                    episode_started_at: episode.started_at,
                    last_error: episode.last_error,
                    failed_runs: episode.failed_runs,
                })
            })
            .collect::<Vec<_>>();
        episodes.sort_by_key(|episode| (episode.project_id, episode.workspace_id));
        episodes
    }

    /// One notice per project whose primary graph has been Degraded or Failed
    /// for over [`CODEGRAPH_UNHEALTHY_NOTICE_SECS`] (#2246), in project order.
    /// Sandbox and worktree checkouts are not the project's graph, so a
    /// checkout's failure does not raise a project notice.
    #[must_use]
    pub fn unhealthy_notices(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<CodegraphHealthNoticeV1> {
        let primaries = self.slots.lock().map_or_else(
            |_| HashSet::new(),
            |slots| {
                slots
                    .iter()
                    .filter(|(_, slot)| {
                        matches!(slot.workspace.instance, WorkspaceInstanceKey::Primary)
                    })
                    .map(|(id, _)| *id)
                    .collect::<HashSet<_>>()
            },
        );
        let mut notices = self.statuses.lock().map_or_else(
            |_| Vec::new(),
            |statuses| {
                statuses
                    .iter()
                    .filter(|(id, _)| primaries.contains(id))
                    .filter_map(|(_, status)| {
                        status.unhealthy_notice(now, CODEGRAPH_UNHEALTHY_NOTICE_SECS)
                    })
                    .collect::<Vec<_>>()
            },
        );
        notices.sort_by_key(|notice| (notice.project_id, notice.workspace_id));
        notices
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
        self.reconcile_seeded(workspaces, HashMap::new())
    }

    /// The startup registration (#2375): the first snapshot is applied off the
    /// request-readiness path. Each registered project's index database is
    /// opened and its interrupted runs recovered before any slot is
    /// published, and a workspace with a persisted status starts from it, as
    /// when the manager was constructed with the snapshot.
    ///
    /// # Errors
    /// Returns an error if a project index cannot be opened or the snapshot
    /// exceeds the registry bounds.
    pub fn install_startup(&self, workspaces: Vec<RegisteredWorkspace>) -> Result<()> {
        if workspaces.len() > MAX_REGISTERED_WORKSPACES {
            return Err(IndexError::DiscoveryLimit("registered workspaces"));
        }
        let seeded = durable_statuses(&self.index_root, &workspaces)?;
        self.reconcile_seeded(workspaces, seeded)
    }

    fn reconcile_seeded(
        &self,
        workspaces: Vec<RegisteredWorkspace>,
        mut seeded: HashMap<Uuid, IndexStatus>,
    ) -> Result<()> {
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
                let status = seeded
                    .remove(id)
                    .unwrap_or_else(|| IndexStatus::new(slot.workspace.project_id, *id));
                statuses.insert(*id, status);
            }
        }
        *slots = next;
        Ok(())
    }
}

/// Opens each registered project's index database (recovering interrupted
/// runs) and returns the persisted status of every workspace that has one.
fn durable_statuses(
    index_root: &Path,
    workspaces: &[RegisteredWorkspace],
) -> Result<HashMap<Uuid, IndexStatus>> {
    let mut stores = HashMap::new();
    for project_id in workspaces.iter().map(|workspace| workspace.project_id) {
        if stores.contains_key(&project_id) {
            continue;
        }
        let db = worker::project_db_path(index_root, project_id);
        if db.exists() {
            let mut store = rsi_codegraph::CodegraphStore::open(&db, project_id)?;
            store.recover_interrupted_index_runs()?;
            store.generation_details()?;
            stores.insert(project_id, store);
        }
    }
    let mut statuses = HashMap::new();
    for workspace in workspaces {
        let id = workspace.workspace_id;
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
    }
    Ok(statuses)
}

pub struct IndexManager {
    index_root: PathBuf,
    rx: mpsc::Receiver<Uuid>,
    slots: Arc<Mutex<HashMap<Uuid, Arc<Slot>>>>,
    statuses: Arc<Mutex<HashMap<Uuid, IndexStatus>>>,
    worker_states: HashMap<Uuid, WorkerState>,
    worker_roots: HashMap<Uuid, PathBuf>,
    /// #2464: each workspace's last publication digests and the root they
    /// describe. They outlive the resident extraction cache, so a workspace
    /// whose cache was dropped for another workspace's pass still recognises
    /// unchanged input instead of re-extracting and republishing it.
    published: HashMap<Uuid, (PathBuf, worker::PublishedDigests)>,
    pending: VecDeque<Uuid>,
    pending_set: HashSet<Uuid>,
    backoff: HashMap<Uuid, FailureBackoff>,
    events: Option<Arc<EventBus>>,
    enabled: Arc<AtomicBool>,
    #[cfg(test)]
    resident_count: Arc<std::sync::atomic::AtomicUsize>,
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
        let statuses = durable_statuses(&index_root, &workspaces)?;
        let mut slots = HashMap::new();
        for workspace in workspaces {
            let id = workspace.workspace_id;
            if slots.contains_key(&id) {
                return Err(IndexError::UnsafeWorkspace(
                    "duplicate workspace identity".into(),
                ));
            }
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
        #[cfg(test)]
        let resident_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handle = IndexHandle {
            index_root: index_root.clone(),
            tx,
            slots: slots.clone(),
            statuses: statuses.clone(),
            enabled: enabled.clone(),
            #[cfg(test)]
            resident_count: resident_count.clone(),
        };
        let manager = Self {
            index_root,
            rx,
            slots,
            statuses,
            worker_states: HashMap::new(),
            worker_roots: HashMap::new(),
            published: HashMap::new(),
            pending: VecDeque::new(),
            pending_set: HashSet::new(),
            backoff: HashMap::new(),
            events: None,
            enabled,
            #[cfg(test)]
            resident_count,
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

    /// Fold one finished run into the workspace's failure streak. A success
    /// or a transient outcome ends the streak; a repeat of the same real
    /// failure extends it and, past [`BACKOFF_FREE_FAILURES`], delays retries.
    fn settle_backoff(&mut self, id: Uuid, failed: bool, class: Option<String>) {
        let Some(class) = class.filter(|_| failed) else {
            self.backoff.remove(&id);
            return;
        };
        let next = FailureBackoff::record(self.backoff.get(&id), class, Instant::now());
        if next.count == BACKOFF_FREE_FAILURES {
            tracing::warn!(
                workspace_id = %id,
                failure = %next.class,
                "codegraph index failed the same way repeatedly; backing off retries (#2246)"
            );
        }
        self.backoff.insert(id, next);
    }

    /// Queue the workspaces whose request was held back and whose delay ended.
    fn release_expired_backoffs(&mut self) {
        let now = Instant::now();
        let due = self
            .backoff
            .iter_mut()
            .filter(|(_, backoff)| backoff.deferred && !backoff.active(now))
            .map(|(id, backoff)| {
                backoff.deferred = false;
                *id
            })
            .collect::<Vec<_>>();
        for id in due {
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
                // An operator turning indexing off and on is an explicit retry.
                self.backoff.clear();
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
            self.release_expired_backoffs();
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
            if let Some(backoff) = self.backoff.get_mut(&id)
                && backoff.active(Instant::now())
            {
                // The request is kept, not lost: `release_expired_backoffs`
                // queues it when the delay ends.
                backoff.deferred = true;
                continue;
            }
            let observed_revision = slot.revision.load(Ordering::Acquire);
            let workspace = slot.workspace.clone();
            let db = worker::project_db_path(&self.index_root, workspace.project_id);
            let mut state = if self.worker_roots.get(&id) == Some(&workspace.root) {
                self.worker_states.remove(&id).unwrap_or_default()
            } else {
                self.published
                    .get(&id)
                    .filter(|(root, _)| *root == workspace.root)
                    .map_or_else(WorkerState::default, |(_, digests)| {
                        WorkerState::from_published_digests(digests.clone())
                    })
            };
            // Drop every other resident cache before this run builds its own,
            // so a pass never holds another workspace's ~340 MB on top of its
            // own ~750 MB extraction peak (#960 dhat probe).
            self.worker_states.clear();
            self.worker_roots.clear();
            self.worker_roots.insert(id, workspace.root.clone());
            self.update_status(id, |status| {
                status.phase = IndexPhase::Building;
                status.pending_rescan = false;
            });
            let started = Instant::now();
            let run_root = workspace.root.clone();
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
                // A pass frees most of its ~750 MB peak here; return those
                // arena pages instead of keeping them resident (#960).
                crate::process_memory::release_free_memory();
                (state, result, ready_on_error)
            })
            .await;
            let (result, ready_on_error) = match outcome {
                Ok((state, result, ready_on_error)) => {
                    self.published
                        .insert(id, (run_root, state.published_digests()));
                    // The run began with no other resident state, and this
                    // insert is the only one, so at most one cache stays live.
                    self.worker_states.insert(id, state);
                    debug_assert!(self.worker_states.len() <= MAX_RESIDENT_WORKER_STATES);
                    #[cfg(test)]
                    self.resident_count
                        .store(self.worker_states.len(), Ordering::Relaxed);
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
                self.published.remove(&id);
                self.queue_rescans();
                continue;
            }
            let class = result.as_ref().err().and_then(failure_class);
            let failed = result.is_err();
            self.settle_backoff(id, failed, class.clone());
            self.update_status(id, |status| {
                status.note_run(failed, class.is_some(), chrono::Utc::now());
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
            self.published.retain(|id, _| registered.contains(id));
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    /// #2375: the deferred startup registration publishes the snapshot with
    /// the persisted status, exactly as constructing the manager with it does.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn install_startup_seeds_the_durable_status_after_recovery() {
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
            IndexManager::new(indexes.path().to_path_buf(), Vec::new()).unwrap();
        assert!(handle.status(workspace.workspace_id).is_none());
        handle.install_startup(vec![workspace.clone()]).unwrap();
        let status = handle.status(workspace.workspace_id).unwrap();
        assert_eq!(status.phase, IndexPhase::Recovering);
        assert!(status.pending_rescan);
        assert_eq!(status.ready.unwrap().snapshot_digest, ready.snapshot_digest);
        assert_eq!(
            handle
                .registered_project_workspaces(workspace.project_id)
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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
        // #2351: the head's identities are read from its blocks. The ledger
        // holds only identities that left every retained generation, so
        // repairing a head's accounting writes none.
        let connection = rusqlite::Connection::open(&db).unwrap();
        let count = |sql: &str| -> i64 {
            connection
                .query_row(
                    sql,
                    rusqlite::params![workspace.workspace_id.to_string(), ready.generation],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(
            count("SELECT count(*) FROM cg_retained_nodes WHERE workspace_id=?1 AND ?2=?2"),
            0
        );
        assert!(
            count("SELECT count(*) FROM cg_nodes WHERE workspace_id=?1 AND generation=?2") > 0,
            "the head's nodes are readable from its blocks"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn sequential_indices_keep_one_resident_state() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(b.path().join("lib.rs"), "pub fn b() {}\n").unwrap();
        let first = RegisteredWorkspace::primary(Uuid::new_v4(), a.path()).unwrap();
        let second = RegisteredWorkspace::primary(Uuid::new_v4(), b.path()).unwrap();
        let (manager, handle) = IndexManager::new(
            indexes.path().to_path_buf(),
            vec![first.clone(), second.clone()],
        )
        .unwrap();
        handle.set_enabled(true);
        let task = tokio::spawn(manager.run());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let a_status = handle.status(first.workspace_id).unwrap();
                let b_status = handle.status(second.workspace_id).unwrap();
                if a_status.phase == IndexPhase::Ready && b_status.phase == IndexPhase::Ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        // Both workspaces reached Ready.
        let a_store = rsi_codegraph::CodegraphStore::open(
            &worker::project_db_path(indexes.path(), first.project_id),
            first.project_id,
        )
        .unwrap();
        let b_store = rsi_codegraph::CodegraphStore::open(
            &worker::project_db_path(indexes.path(), second.project_id),
            second.project_id,
        )
        .unwrap();
        let a_ready = a_store.current_ready(first.workspace_id).unwrap();
        let b_ready = b_store.current_ready(second.workspace_id).unwrap();
        assert_eq!(a_ready.workspace_id, first.workspace_id);
        assert_eq!(b_ready.workspace_id, second.workspace_id);
        // The retention gate fires after each index, so at most one state is resident.
        let resident = handle.resident_worker_state_count();
        assert_eq!(resident, 1);
        drop(handle);
        task.abort();
    }

    /// #2464: with two workspaces only one extraction cache stays resident,
    /// yet a rescan of an unchanged workspace whose cache was dropped confirms
    /// its head instead of re-extracting and publishing an identical
    /// generation (the hub re-extracted 9,063 unchanged files per pass, and
    /// every identical generation then had to be pruned).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn evicted_unchanged_workspace_rescans_without_republishing_2464() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(b.path().join("lib.rs"), "pub fn b() {}\n").unwrap();
        let first = RegisteredWorkspace::primary(Uuid::new_v4(), a.path()).unwrap();
        let second = RegisteredWorkspace::primary(Uuid::new_v4(), b.path()).unwrap();
        let (manager, handle) = IndexManager::new(
            indexes.path().to_path_buf(),
            vec![first.clone(), second.clone()],
        )
        .unwrap();
        handle.set_enabled(true);
        let task = tokio::spawn(manager.run());
        let wait = |workspace: Uuid, done: fn(&IndexStatus) -> bool| {
            let handle = handle.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if handle.status(workspace).is_some_and(|status| done(&status)) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("status {:?}", handle.status(workspace)));
            }
        };
        let ready = |status: &IndexStatus| status.phase == IndexPhase::Ready;
        let confirmed = |status: &IndexStatus| {
            status.phase == IndexPhase::Ready
                && !status.pending_rescan
                && status.files_extracted == 0
                && status.files_reused == 1
                && !status.last_published
        };
        wait(first.workspace_id, ready).await;
        wait(second.workspace_id, ready).await;
        // Whichever ran last is resident; rescanning one then the other runs
        // at least one of them from digests alone.
        for workspace in [&first, &second] {
            handle.request(workspace.workspace_id).unwrap();
            wait(workspace.workspace_id, confirmed).await;
        }
        assert_eq!(handle.resident_worker_state_count(), 1);
        for workspace in [&first, &second] {
            let store = rsi_codegraph::CodegraphStore::open(
                worker::project_db_path(indexes.path(), workspace.project_id),
                workspace.project_id,
            )
            .unwrap();
            assert_eq!(
                store
                    .current_ready(workspace.workspace_id)
                    .unwrap()
                    .generation,
                1,
                "an unchanged workspace keeps its first generation"
            );
        }
        drop(handle);
        task.abort();
    }

    fn manager_with_primary() -> (IndexManager, IndexHandle, Uuid, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let project = Uuid::new_v4();
        let workspace = RegisteredWorkspace::primary(project, root.path()).unwrap();
        let id = workspace.workspace_id;
        let index_root = root.path().join("index");
        let (manager, handle) = IndexManager::new(index_root, vec![workspace]).unwrap();
        (manager, handle, id, root)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn identical_failures_back_off_then_a_success_resets_2246() {
        let (mut manager, _handle, id, _root) = manager_with_primary();
        let symlink = || Some("source path is a symlink".to_string());
        for _ in 0..(BACKOFF_FREE_FAILURES - 1) {
            manager.settle_backoff(id, true, symlink());
            assert!(!manager.backoff[&id].active(Instant::now()), "free retries");
        }
        manager.settle_backoff(id, true, symlink());
        assert!(manager.backoff[&id].active(Instant::now()), "backed off");
        let first = manager.backoff[&id].until.unwrap();
        manager.settle_backoff(id, true, symlink());
        assert!(
            manager.backoff[&id].until.unwrap() > first + BACKOFF_BASE / 2,
            "the delay grows with the streak"
        );
        // A different failure restarts the streak at one.
        manager.settle_backoff(id, true, Some("other".into()));
        assert_eq!(manager.backoff[&id].count, 1);
        assert!(!manager.backoff[&id].active(Instant::now()));
        // A transient outcome and a success both end it.
        manager.settle_backoff(id, true, symlink());
        manager.settle_backoff(id, true, None);
        assert!(!manager.backoff.contains_key(&id));
        manager.settle_backoff(id, true, symlink());
        manager.settle_backoff(id, false, None);
        assert!(!manager.backoff.contains_key(&id));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn backoff_delay_is_capped_and_failure_classes_ignore_transients_2246() {
        assert_eq!(FailureBackoff::delay(BACKOFF_FREE_FAILURES - 1), None);
        assert_eq!(
            FailureBackoff::delay(BACKOFF_FREE_FAILURES),
            Some(BACKOFF_BASE)
        );
        assert_eq!(FailureBackoff::delay(u32::MAX), Some(BACKOFF_CAP));
        assert_eq!(failure_class(&IndexError::Superseded), None);
        assert_eq!(failure_class(&IndexError::ActiveReader), None);
        assert_eq!(failure_class(&IndexError::SourceChanged), None);
        assert_eq!(
            failure_class(&IndexError::DiskBudget {
                used_bytes: 1,
                ceiling_bytes: 2
            }),
            failure_class(&IndexError::DiskBudget {
                used_bytes: 5,
                ceiling_bytes: 2
            })
        );
        assert!(failure_class(&IndexError::UnsafeWorkspace("x".into())).is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn deferred_request_is_queued_when_the_backoff_ends_2246() {
        let (mut manager, _handle, id, _root) = manager_with_primary();
        manager.backoff.insert(
            id,
            FailureBackoff {
                class: "x".into(),
                count: 9,
                until: Some(Instant::now() + Duration::from_secs(3600)),
                deferred: true,
            },
        );
        manager.release_expired_backoffs();
        assert!(manager.pending.is_empty(), "still backed off");
        manager.backoff.get_mut(&id).unwrap().until = Some(Instant::now());
        manager.release_expired_backoffs();
        assert_eq!(manager.pending.front(), Some(&id));
        assert!(!manager.backoff[&id].deferred);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn unhealthy_notice_needs_a_primary_graph_bad_for_over_a_day_2246() {
        let (_manager, handle, id, _root) = manager_with_primary();
        let now = chrono::Utc::now();
        let day = rsi_common::codegraph::CODEGRAPH_UNHEALTHY_NOTICE_SECS;
        let set = |phase, since: Option<chrono::DateTime<chrono::Utc>>| {
            let mut statuses = handle.statuses.lock().unwrap();
            let status = statuses.get_mut(&id).unwrap();
            status.phase = phase;
            status.unhealthy_since = since;
            status.last_error = Some("source path is a symlink".into());
            status.failed_runs = 7;
        };
        set(
            IndexPhase::Failed,
            Some(now - chrono::Duration::seconds(day - 60)),
        );
        assert!(handle.unhealthy_notices(now).is_empty(), "under a day");
        set(
            IndexPhase::Failed,
            Some(now - chrono::Duration::seconds(day + 60)),
        );
        let notices = handle.unhealthy_notices(now);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].phase, "failed");
        assert_eq!(notices[0].failed_runs, 7);
        assert_eq!(
            notices[0].last_error.as_deref(),
            Some("source path is a symlink")
        );
        set(
            IndexPhase::Degraded,
            Some(now - chrono::Duration::seconds(day + 60)),
        );
        assert_eq!(handle.unhealthy_notices(now)[0].phase, "degraded");
        set(IndexPhase::Ready, None);
        assert!(handle.unhealthy_notices(now).is_empty(), "healthy");
        // A success clears the clock.
        handle
            .statuses
            .lock()
            .unwrap()
            .get_mut(&id)
            .unwrap()
            .note_run(false, false, now);
        assert!(handle.status(id).unwrap().unhealthy_since.is_none());
    }
}
