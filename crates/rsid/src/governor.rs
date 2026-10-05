//! Resource governor (#1014): typed admission for builds and landers.
//!
//! Replaces the `~/.rsi/bin/cargo-slot` shell gate. A local client (the repo
//! `scripts/cargo-slot`) asks for a slot of a class, polls while it is queued,
//! and releases the slot when its command ends. The daemon owns the policy
//! (slots per class, load, disk and memory gates), the FIFO queue, and the
//! leases. A lease is tied to the client process (pid + start time from
//! `/proc`), so a killed client never leaks a slot: every call reaps leases
//! and waiters whose holder is gone.
//!
//! The gates keep the cargo-slot semantics exactly:
//! * disk: `/` must have at least `min_free_disk_gb` free;
//! * load: the 1-minute load must be below `max_load` (0 = 1.25 x cores);
//! * memory: `MemAvailable` at least `min_avail_mem_gb`, and the workers
//!   slice's anonymous + shmem memory (from `memory.stat`, never
//!   `memory.current`, which counts reclaimable page cache) below
//!   `max_workers_slice_gb`;
//! * slots: at most `build_slots` / `lander_slots` leases of each class.
//!
//! A slot is the only thing a lease grants; it carries no other authority.

pub use crate::store_support::config_types::GovernorPolicy;
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const GIB: f64 = 1_073_741_824.0;

/// A queued (not yet granted) ticket that has not been polled for this long is
/// dropped: its client is gone or wedged.
pub const WAITER_STALE: Duration = Duration::from_secs(120);

/// Admission class. Landers (`rsi-rolling-land` runs) and builds (everything
/// else) draw on separate slot pools so landers cannot starve builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionClass {
    Build,
    Lander,
}

impl AdmissionClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Lander => "lander",
        }
    }
}

/// One reading of the host resources the gates compare against the policy.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ResourceSample {
    pub load1: f64,
    pub cores: u32,
    pub disk_free_gb: f64,
    pub mem_available_gb: f64,
    /// Anonymous + shmem memory of the workers slice; `None` when the cgroup
    /// file is unreadable (the gate then passes, as cargo-slot did).
    pub workers_slice_anon_gb: Option<f64>,
}

/// Why a request cannot be admitted right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "gate", rename_all = "snake_case")]
pub enum BlockReason {
    Disk {
        free_gb: f64,
        min_gb: u64,
    },
    Load {
        load1: f64,
        max: u32,
    },
    MemoryAvailable {
        available_gb: f64,
        min_gb: u64,
    },
    WorkersSlice {
        used_gb: f64,
        max_gb: u64,
    },
    Slots {
        class: AdmissionClass,
        in_use: u32,
        cap: u32,
    },
    QueueAhead {
        class: AdmissionClass,
        ahead: u32,
    },
}

impl BlockReason {
    /// One-line operator text, also printed by the client while it waits.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Disk { free_gb, min_gb } => {
                format!("only {free_gb:.0} GB free on / (< {min_gb}); waiting for space")
            }
            Self::Load { load1, max } => format!("load {load1:.1} >= {max}; waiting"),
            Self::MemoryAvailable {
                available_gb,
                min_gb,
            } => {
                format!("memory low (available {available_gb:.0} GB < {min_gb}); waiting")
            }
            Self::WorkersSlice { used_gb, max_gb } => {
                format!("workers slice uses {used_gb:.0} GB anon+shmem (>= {max_gb}); waiting")
            }
            Self::Slots { class, in_use, cap } => {
                format!("all {cap} {} slots busy ({in_use} in use)", class.as_str())
            }
            Self::QueueAhead { class, ahead } => {
                format!("{ahead} {} request(s) queued ahead", class.as_str())
            }
        }
    }
}

/// Identity of the client process a lease or ticket is tied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    /// `/proc/<pid>/stat` field 22, guarding against pid reuse.
    pub start_ticks: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquireParams {
    pub class: AdmissionClass,
    /// Client process to bind the lease to (the wrapper's own pid).
    pub pid: u32,
    /// Free-form label for the queue view (e.g. the command line, truncated).
    #[serde(default)]
    pub label: Option<String>,
    /// Ticket from a previous `queued` reply; omit on the first call.
    #[serde(default)]
    pub ticket_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseParams {
    /// The lease id (equal to the ticket id); a queued ticket is cancelled.
    pub lease_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AcquireOutcome {
    Granted {
        lease_id: Uuid,
        class: AdmissionClass,
    },
    Queued {
        ticket_id: Uuid,
        class: AdmissionClass,
        /// 0 is the head of the class queue.
        position: u32,
        reason: BlockReason,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntryView {
    pub ticket_id: Uuid,
    pub class: AdmissionClass,
    pub position: u32,
    pub pid: u32,
    pub label: Option<String>,
    pub enqueued_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseView {
    pub lease_id: Uuid,
    pub class: AdmissionClass,
    pub pid: u32,
    pub label: Option<String>,
    pub granted_at: DateTime<Utc>,
}

/// One gate with its current value and threshold, for health output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateMargin {
    pub gate: String,
    pub value: f64,
    pub threshold: f64,
    /// `true` when the gate currently admits new work.
    pub open: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernorSnapshot {
    pub policy: GovernorPolicy,
    pub sample: ResourceSample,
    pub margins: Vec<GateMargin>,
    pub build_in_use: u32,
    pub lander_in_use: u32,
    pub leases: Vec<LeaseView>,
    pub queue: Vec<QueueEntryView>,
}

struct Waiter {
    ticket: Uuid,
    class: AdmissionClass,
    holder: Holder,
    label: Option<String>,
    enqueued_at: DateTime<Utc>,
    last_polled: Instant,
}

struct Lease {
    id: Uuid,
    class: AdmissionClass,
    holder: Holder,
    label: Option<String>,
    granted_at: DateTime<Utc>,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Waiter>,
    leases: HashMap<Uuid, Lease>,
}

type SampleFn = Box<dyn Fn() -> ResourceSample + Send + Sync>;
type HolderFn = Box<dyn Fn(u32) -> Option<Holder> + Send + Sync>;

pub struct Governor {
    state: Mutex<State>,
    sample: SampleFn,
    /// Current identity of a pid, or `None` when it is gone. A lease is live
    /// only while this equals the identity recorded at acquire time.
    resolve_holder: HolderFn,
    /// A disk-floor stall was already logged this episode (#932).
    disk_warned: std::sync::atomic::AtomicBool,
}

impl Governor {
    #[must_use]
    pub fn new(sample: SampleFn, resolve_holder: HolderFn) -> Self {
        Self {
            state: Mutex::new(State::default()),
            sample,
            resolve_holder,
            disk_warned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The process-wide governor with real `/proc` and cgroup probes.
    #[must_use]
    pub fn global() -> &'static Self {
        static GLOBAL: LazyLock<Governor> =
            LazyLock::new(|| Governor::new(Box::new(sample_host), Box::new(resolve_proc_holder)));
        &GLOBAL
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask for a slot. Idempotent per `ticket_id`.
    pub fn acquire(
        &self,
        policy: &GovernorPolicy,
        params: &AcquireParams,
    ) -> Result<AcquireOutcome, String> {
        let sample = (self.sample)();
        if sample.disk_free_gb >= policy.min_free_disk_gb as f64 {
            self.disk_warned
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
        let mut state = self.state();
        self.reap(&mut state);
        if let Some(ticket) = params.ticket_id {
            if let Some(lease) = state.leases.get(&ticket) {
                return Ok(AcquireOutcome::Granted {
                    lease_id: lease.id,
                    class: lease.class,
                });
            }
            if let Some(waiter) = state.queue.iter_mut().find(|w| w.ticket == ticket) {
                waiter.last_polled = Instant::now();
            } else {
                return Err("unknown or expired ticket; acquire again without ticket_id".into());
            }
        } else {
            let holder = (self.resolve_holder)(params.pid)
                .ok_or_else(|| format!("pid {} is not visible to the daemon", params.pid))?;
            state.queue.push_back(Waiter {
                ticket: Uuid::new_v4(),
                class: params.class,
                holder,
                label: params.label.as_ref().map(|l| l.chars().take(200).collect()),
                enqueued_at: Utc::now(),
                last_polled: Instant::now(),
            });
        }
        let ticket = params.ticket_id.unwrap_or_else(|| {
            state
                .queue
                .back()
                .map_or_else(Uuid::nil, |waiter| waiter.ticket)
        });
        Self::admit(&mut state, policy, &sample);
        if let Some(lease) = state.leases.get(&ticket) {
            return Ok(AcquireOutcome::Granted {
                lease_id: lease.id,
                class: lease.class,
            });
        }
        let (class, position) = Self::position_of(&state, ticket)
            .ok_or_else(|| "ticket vanished during admission".to_string())?;
        let reason = Self::reason_for(&state, policy, &sample, class, position);
        self.note_disk_stall(policy, &sample, &reason);
        Ok(AcquireOutcome::Queued {
            ticket_id: ticket,
            class,
            position,
            message: reason.describe(),
            reason,
        })
    }

    /// Log one warning per episode when the disk floor stalls admission, so a
    /// silent stall of every landing is visible in the daemon log (#932). The
    /// client prints the same reason; health shows the gate margin.
    fn note_disk_stall(
        &self,
        policy: &GovernorPolicy,
        sample: &ResourceSample,
        reason: &BlockReason,
    ) {
        use std::sync::atomic::Ordering;
        if matches!(reason, BlockReason::Disk { .. })
            && !self.disk_warned.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                free_gb = sample.disk_free_gb,
                min_gb = policy.min_free_disk_gb,
                "Resource governor: cargo/lander admission is stalled by the disk floor; \
                 reclaim stale lander scratch and agent scratch (GetHealthStatus shows margins)"
            );
        }
    }

    /// Whether a disk-floor stall warning is currently latched.
    #[cfg(test)]
    fn disk_warning_latched(&self) -> bool {
        self.disk_warned.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Release a lease or cancel a queued ticket. Returns whether it existed.
    pub fn release(&self, policy: &GovernorPolicy, lease_id: Uuid) -> bool {
        let sample = (self.sample)();
        let mut state = self.state();
        let mut existed = state.leases.remove(&lease_id).is_some();
        let before = state.queue.len();
        state.queue.retain(|w| w.ticket != lease_id);
        existed |= state.queue.len() != before;
        self.reap(&mut state);
        Self::admit(&mut state, policy, &sample);
        existed
    }

    /// Current policy margins, leases and queue.
    pub fn snapshot(&self, policy: &GovernorPolicy) -> GovernorSnapshot {
        let sample = (self.sample)();
        let mut state = self.state();
        self.reap(&mut state);
        Self::admit(&mut state, policy, &sample);
        let count = |class| {
            u32::try_from(state.leases.values().filter(|l| l.class == class).count())
                .unwrap_or(u32::MAX)
        };
        let mut leases: Vec<LeaseView> = state
            .leases
            .values()
            .map(|l| LeaseView {
                lease_id: l.id,
                class: l.class,
                pid: l.holder.pid,
                label: l.label.clone(),
                granted_at: l.granted_at,
            })
            .collect();
        leases.sort_by_key(|l| l.granted_at);
        let mut per_class: HashMap<AdmissionClass, u32> = HashMap::new();
        let queue = state
            .queue
            .iter()
            .map(|w| {
                let slot = per_class.entry(w.class).or_insert(0);
                let position = *slot;
                *slot += 1;
                QueueEntryView {
                    ticket_id: w.ticket,
                    class: w.class,
                    position,
                    pid: w.holder.pid,
                    label: w.label.clone(),
                    enqueued_at: w.enqueued_at,
                }
            })
            .collect();
        GovernorSnapshot {
            policy: *policy,
            margins: margins(policy, &sample),
            sample,
            build_in_use: count(AdmissionClass::Build),
            lander_in_use: count(AdmissionClass::Lander),
            leases,
            queue,
        }
    }

    /// Drop leases and waiters whose client process is gone or stale.
    fn reap(&self, state: &mut State) {
        let alive = |holder: &Holder| (self.resolve_holder)(holder.pid) == Some(*holder);
        state.leases.retain(|_, lease| alive(&lease.holder));
        state
            .queue
            .retain(|w| alive(&w.holder) && w.last_polled.elapsed() < WAITER_STALE);
    }

    /// Grant slots to queue heads, FIFO within each class.
    fn admit(state: &mut State, policy: &GovernorPolicy, sample: &ResourceSample) {
        if global_block(policy, sample).is_some() {
            return;
        }
        let mut blocked: Vec<AdmissionClass> = Vec::new();
        let mut index = 0;
        while index < state.queue.len() {
            let class = state.queue[index].class;
            if blocked.contains(&class) {
                index += 1;
                continue;
            }
            let in_use = class_in_use(state, class);
            if in_use >= class_cap(policy, class) {
                blocked.push(class);
                index += 1;
                continue;
            }
            if let Some(waiter) = state.queue.remove(index) {
                state.leases.insert(
                    waiter.ticket,
                    Lease {
                        id: waiter.ticket,
                        class,
                        holder: waiter.holder,
                        label: waiter.label,
                        granted_at: Utc::now(),
                    },
                );
            }
        }
    }

    fn position_of(state: &State, ticket: Uuid) -> Option<(AdmissionClass, u32)> {
        let waiter = state.queue.iter().find(|w| w.ticket == ticket)?;
        let ahead = state
            .queue
            .iter()
            .take_while(|w| w.ticket != ticket)
            .filter(|w| w.class == waiter.class)
            .count();
        Some((waiter.class, u32::try_from(ahead).unwrap_or(u32::MAX)))
    }

    fn reason_for(
        state: &State,
        policy: &GovernorPolicy,
        sample: &ResourceSample,
        class: AdmissionClass,
        position: u32,
    ) -> BlockReason {
        if let Some(reason) = global_block(policy, sample) {
            return reason;
        }
        if position > 0 {
            return BlockReason::QueueAhead {
                class,
                ahead: position,
            };
        }
        BlockReason::Slots {
            class,
            in_use: class_in_use(state, class),
            cap: class_cap(policy, class),
        }
    }
}

fn class_in_use(state: &State, class: AdmissionClass) -> u32 {
    u32::try_from(state.leases.values().filter(|l| l.class == class).count()).unwrap_or(u32::MAX)
}

fn class_cap(policy: &GovernorPolicy, class: AdmissionClass) -> u32 {
    match class {
        AdmissionClass::Build => policy.build_slots,
        AdmissionClass::Lander => policy.lander_slots,
    }
}

/// The load ceiling in effect: the policy value, or 1.25 x cores when 0.
#[must_use]
pub fn effective_max_load(policy: &GovernorPolicy, cores: u32) -> u32 {
    if policy.max_load == 0 {
        cores.saturating_mul(5) / 4
    } else {
        policy.max_load
    }
}

/// First closed host gate, in cargo-slot order: disk, load, memory, slice.
fn global_block(policy: &GovernorPolicy, sample: &ResourceSample) -> Option<BlockReason> {
    if sample.disk_free_gb < policy.min_free_disk_gb as f64 {
        return Some(BlockReason::Disk {
            free_gb: sample.disk_free_gb,
            min_gb: policy.min_free_disk_gb,
        });
    }
    let max_load = effective_max_load(policy, sample.cores);
    if sample.load1 >= f64::from(max_load) {
        return Some(BlockReason::Load {
            load1: sample.load1,
            max: max_load,
        });
    }
    if sample.mem_available_gb < policy.min_avail_mem_gb as f64 {
        return Some(BlockReason::MemoryAvailable {
            available_gb: sample.mem_available_gb,
            min_gb: policy.min_avail_mem_gb,
        });
    }
    if let Some(used) = sample.workers_slice_anon_gb
        && used >= policy.max_workers_slice_gb as f64
    {
        return Some(BlockReason::WorkersSlice {
            used_gb: used,
            max_gb: policy.max_workers_slice_gb,
        });
    }
    None
}

fn margins(policy: &GovernorPolicy, sample: &ResourceSample) -> Vec<GateMargin> {
    let max_load = f64::from(effective_max_load(policy, sample.cores));
    let mut out = vec![
        GateMargin {
            gate: "disk_free_gb".into(),
            value: sample.disk_free_gb,
            threshold: policy.min_free_disk_gb as f64,
            open: sample.disk_free_gb >= policy.min_free_disk_gb as f64,
        },
        GateMargin {
            gate: "load1".into(),
            value: sample.load1,
            threshold: max_load,
            open: sample.load1 < max_load,
        },
        GateMargin {
            gate: "mem_available_gb".into(),
            value: sample.mem_available_gb,
            threshold: policy.min_avail_mem_gb as f64,
            open: sample.mem_available_gb >= policy.min_avail_mem_gb as f64,
        },
    ];
    if let Some(used) = sample.workers_slice_anon_gb {
        out.push(GateMargin {
            gate: "workers_slice_anon_gb".into(),
            value: used,
            threshold: policy.max_workers_slice_gb as f64,
            open: used < policy.max_workers_slice_gb as f64,
        });
    }
    out
}

/// Identity of `pid` from `/proc/<pid>/stat`, or `None` when it is gone.
#[must_use]
pub fn resolve_proc_holder(pid: u32) -> Option<Holder> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let start_ticks = parse_start_ticks(&stat)?;
    Some(Holder { pid, start_ticks })
}

/// Field 22 of `/proc/<pid>/stat`. The command name (field 2) may contain
/// spaces and parentheses, so parse after the last `)`.
pub(crate) fn parse_start_ticks(stat: &str) -> Option<u64> {
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    // `rest` begins at field 3 (state); starttime is field 22 = index 19.
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Anonymous + shmem bytes of a `memory.stat` body. Deliberately ignores the
/// page cache (`file`), which the kernel reclaims before it kills anything.
#[must_use]
pub fn anon_shmem_bytes(memory_stat: &str) -> u64 {
    memory_stat
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(' ')?;
            matches!(key, "anon" | "shmem")
                .then(|| value.trim().parse::<u64>().ok())
                .flatten()
        })
        .sum()
}

fn mem_available_kib(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

fn workers_slice_stat_path() -> std::path::PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/rsi.slice/rsi-workers.slice/memory.stat"
    )
    .into()
}

fn sample_host() -> ResourceSample {
    let load1 = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|body| body.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);
    let cores = std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1);
    let disk_free_gb = nix::sys::statvfs::statvfs("/").map_or(f64::MAX, |stats| {
        stats.blocks_available() as f64 * stats.fragment_size() as f64 / GIB
    });
    let mem_available_gb = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|body| mem_available_kib(&body))
        .map_or(f64::MAX, |kib| kib as f64 * 1024.0 / GIB);
    let workers_slice_anon_gb = std::fs::read_to_string(workers_slice_stat_path())
        .ok()
        .map(|body| anon_shmem_bytes(&body) as f64 / GIB);
    ResourceSample {
        load1,
        cores,
        disk_free_gb,
        mem_available_gb,
        workers_slice_anon_gb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone)]
    struct World {
        sample: Arc<Mutex<ResourceSample>>,
        alive: Arc<Mutex<Vec<u32>>>,
    }

    impl World {
        fn new() -> (Self, Governor) {
            let world = Self {
                sample: Arc::new(Mutex::new(healthy())),
                alive: Arc::new(Mutex::new((1..=100).collect())),
            };
            let s = world.sample.clone();
            let a = world.alive.clone();
            let governor = Governor::new(
                Box::new(move || *s.lock().unwrap()),
                Box::new(move |pid| {
                    a.lock().unwrap().contains(&pid).then_some(Holder {
                        pid,
                        start_ticks: 7,
                    })
                }),
            );
            (world, governor)
        }

        fn kill(&self, pid: u32) {
            self.alive.lock().unwrap().retain(|p| *p != pid);
        }

        fn set(&self, f: impl FnOnce(&mut ResourceSample)) {
            f(&mut self.sample.lock().unwrap());
        }
    }

    fn healthy() -> ResourceSample {
        ResourceSample {
            load1: 1.0,
            cores: 32,
            disk_free_gb: 100.0,
            mem_available_gb: 64.0,
            workers_slice_anon_gb: Some(2.0),
        }
    }

    fn req(class: AdmissionClass, pid: u32) -> AcquireParams {
        AcquireParams {
            class,
            pid,
            label: None,
            ticket_id: None,
        }
    }

    fn poll(
        g: &Governor,
        p: &GovernorPolicy,
        ticket: Uuid,
        class: AdmissionClass,
    ) -> AcquireOutcome {
        g.acquire(
            p,
            &AcquireParams {
                class,
                pid: 0,
                label: None,
                ticket_id: Some(ticket),
            },
        )
        .unwrap()
    }

    fn granted(outcome: &AcquireOutcome) -> Uuid {
        match outcome {
            AcquireOutcome::Granted { lease_id, .. } => *lease_id,
            other => panic!("expected granted, got {other:?}"),
        }
    }

    fn queued(outcome: &AcquireOutcome) -> (Uuid, u32, BlockReason) {
        match outcome {
            AcquireOutcome::Queued {
                ticket_id,
                position,
                reason,
                ..
            } => (*ticket_id, *position, reason.clone()),
            other => panic!("expected queued, got {other:?}"),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn default_policy_equals_cargo_slot_values() {
        let p = GovernorPolicy::default();
        assert_eq!((p.build_slots, p.lander_slots, p.max_load), (4, 5, 0));
        assert_eq!(
            (
                p.min_free_disk_gb,
                p.min_avail_mem_gb,
                p.max_workers_slice_gb
            ),
            (30, 16, 30)
        );
        assert_eq!(effective_max_load(&p, 32), 40);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn fifo_admission_with_position_and_slot_reason() {
        let (_w, g) = World::new();
        let p = GovernorPolicy {
            build_slots: 2,
            ..GovernorPolicy::default()
        };
        let a = granted(&g.acquire(&p, &req(AdmissionClass::Build, 1)).unwrap());
        let _b = granted(&g.acquire(&p, &req(AdmissionClass::Build, 2)).unwrap());
        let (t3, pos3, why3) = queued(&g.acquire(&p, &req(AdmissionClass::Build, 3)).unwrap());
        let (t4, pos4, why4) = queued(&g.acquire(&p, &req(AdmissionClass::Build, 4)).unwrap());
        assert_eq!(pos3, 0);
        assert_eq!(
            why3,
            BlockReason::Slots {
                class: AdmissionClass::Build,
                in_use: 2,
                cap: 2
            }
        );
        assert_eq!(pos4, 1);
        assert_eq!(
            why4,
            BlockReason::QueueAhead {
                class: AdmissionClass::Build,
                ahead: 1
            }
        );
        // Releasing one slot grants the head first; the second is now the head.
        assert!(g.release(&p, a));
        granted(&poll(&g, &p, t3, AdmissionClass::Build));
        let (_, pos, why) = queued(&poll(&g, &p, t4, AdmissionClass::Build));
        assert_eq!(pos, 0);
        assert!(matches!(
            why,
            BlockReason::Slots {
                in_use: 2,
                cap: 2,
                ..
            }
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn classes_use_separate_pools() {
        let (_w, g) = World::new();
        let p = GovernorPolicy {
            build_slots: 1,
            lander_slots: 1,
            ..GovernorPolicy::default()
        };
        granted(&g.acquire(&p, &req(AdmissionClass::Build, 1)).unwrap());
        // A full build pool does not block a lander.
        granted(&g.acquire(&p, &req(AdmissionClass::Lander, 2)).unwrap());
        queued(&g.acquire(&p, &req(AdmissionClass::Build, 3)).unwrap());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn disk_floor_stall_latches_one_warning_until_the_gate_reopens() {
        let (w, g) = World::new();
        let p = GovernorPolicy::default();
        assert!(!g.disk_warning_latched());
        w.set(|s| s.disk_free_gb = 29.0);
        let (ticket, _, _) = queued(&g.acquire(&p, &req(AdmissionClass::Lander, 1)).unwrap());
        assert!(g.disk_warning_latched());
        // Polling again while still stalled keeps the single latched warning.
        queued(&poll(&g, &p, ticket, AdmissionClass::Lander));
        assert!(g.disk_warning_latched());
        w.set(|s| *s = healthy());
        granted(&poll(&g, &p, ticket, AdmissionClass::Lander));
        assert!(!g.disk_warning_latched());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn each_host_gate_blocks_with_its_reason() {
        let p = GovernorPolicy::default();
        let cases: Vec<(Box<dyn Fn(&mut ResourceSample)>, &str)> = vec![
            (Box::new(|s| s.disk_free_gb = 29.0), "disk"),
            (Box::new(|s| s.load1 = 40.0), "load"),
            (Box::new(|s| s.mem_available_gb = 15.0), "memory_available"),
            (
                Box::new(|s| s.workers_slice_anon_gb = Some(30.0)),
                "workers_slice",
            ),
        ];
        for (mutate, gate) in cases {
            let (w, g) = World::new();
            w.set(|s| mutate(s));
            let (ticket, pos, reason) =
                queued(&g.acquire(&p, &req(AdmissionClass::Build, 1)).unwrap());
            assert_eq!(pos, 0);
            let json = serde_json::to_value(&reason).unwrap();
            assert_eq!(json["gate"], gate, "{reason:?}");
            // Reopening the gate admits the queued ticket on the next poll.
            w.set(|s| *s = healthy());
            granted(&poll(&g, &p, ticket, AdmissionClass::Build));
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn gate_thresholds_are_the_cargo_slot_boundaries() {
        let p = GovernorPolicy::default();
        assert!(global_block(&p, &healthy()).is_none());
        let mut s = healthy();
        s.disk_free_gb = 30.0;
        s.load1 = 39.9;
        s.mem_available_gb = 16.0;
        s.workers_slice_anon_gb = Some(29.9);
        assert!(global_block(&p, &s).is_none());
        s.load1 = 40.0;
        assert!(matches!(
            global_block(&p, &s),
            Some(BlockReason::Load { .. })
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn unreadable_workers_slice_does_not_block() {
        let (w, g) = World::new();
        w.set(|s| s.workers_slice_anon_gb = None);
        granted(
            &g.acquire(&GovernorPolicy::default(), &req(AdmissionClass::Build, 1))
                .unwrap(),
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn memory_gate_counts_anon_and_shmem_not_page_cache() {
        let stat = "anon 1073741824\nfile 32212254720\nkernel 5\nshmem 2147483648\nfile_mapped 9\n";
        assert_eq!(anon_shmem_bytes(stat), 3 * 1_073_741_824);
        // 30 GB of reclaimable cache alone contributes nothing.
        assert_eq!(anon_shmem_bytes("file 32212254720\nanon 0\n"), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn lease_is_released_when_the_holder_dies() {
        let (w, g) = World::new();
        let p = GovernorPolicy {
            build_slots: 1,
            ..GovernorPolicy::default()
        };
        granted(&g.acquire(&p, &req(AdmissionClass::Build, 1)).unwrap());
        let (ticket, _, _) = queued(&g.acquire(&p, &req(AdmissionClass::Build, 2)).unwrap());
        assert_eq!(g.snapshot(&p).build_in_use, 1);
        w.kill(1);
        // The dead holder's slot is reaped and the queue head is granted.
        granted(&poll(&g, &p, ticket, AdmissionClass::Build));
        let snap = g.snapshot(&p);
        assert_eq!(snap.build_in_use, 1);
        assert!(snap.leases.iter().all(|l| l.pid == 2));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn dead_waiter_is_dropped_from_the_queue() {
        let (w, g) = World::new();
        let p = GovernorPolicy {
            build_slots: 1,
            ..GovernorPolicy::default()
        };
        granted(&g.acquire(&p, &req(AdmissionClass::Build, 1)).unwrap());
        queued(&g.acquire(&p, &req(AdmissionClass::Build, 2)).unwrap());
        w.kill(2);
        assert!(g.snapshot(&p).queue.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn pid_reuse_is_not_mistaken_for_the_holder() {
        let start = Arc::new(Mutex::new(7_u64));
        let s = start.clone();
        let g = Governor::new(
            Box::new(|| ResourceSample {
                load1: 0.0,
                cores: 8,
                disk_free_gb: 99.0,
                mem_available_gb: 99.0,
                workers_slice_anon_gb: None,
            }),
            Box::new(move |pid| {
                Some(Holder {
                    pid,
                    start_ticks: *s.lock().unwrap(),
                })
            }),
        );
        let p = GovernorPolicy::default();
        granted(&g.acquire(&p, &req(AdmissionClass::Build, 5)).unwrap());
        *start.lock().unwrap() = 8; // same pid, new process
        assert_eq!(g.snapshot(&p).build_in_use, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn release_is_idempotent_and_cancels_queued_tickets() {
        let (_w, g) = World::new();
        let p = GovernorPolicy {
            build_slots: 1,
            ..GovernorPolicy::default()
        };
        let a = granted(&g.acquire(&p, &req(AdmissionClass::Build, 1)).unwrap());
        let (t, _, _) = queued(&g.acquire(&p, &req(AdmissionClass::Build, 2)).unwrap());
        assert!(g.release(&p, t));
        assert!(g.snapshot(&p).queue.is_empty());
        assert!(g.release(&p, a));
        assert!(!g.release(&p, a));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn granted_ticket_is_idempotent_and_unknown_ticket_errors() {
        let (_w, g) = World::new();
        let p = GovernorPolicy::default();
        let a = granted(&g.acquire(&p, &req(AdmissionClass::Build, 1)).unwrap());
        assert_eq!(granted(&poll(&g, &p, a, AdmissionClass::Build)), a);
        let bad = g.acquire(
            &p,
            &AcquireParams {
                class: AdmissionClass::Build,
                pid: 1,
                label: None,
                ticket_id: Some(Uuid::new_v4()),
            },
        );
        assert!(bad.is_err());
        assert!(g.acquire(&p, &req(AdmissionClass::Build, 9999)).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn raising_the_cap_admits_queued_work_on_the_next_poll() {
        let (_w, g) = World::new();
        let one = GovernorPolicy {
            build_slots: 1,
            ..GovernorPolicy::default()
        };
        granted(&g.acquire(&one, &req(AdmissionClass::Build, 1)).unwrap());
        let (t, _, _) = queued(&g.acquire(&one, &req(AdmissionClass::Build, 2)).unwrap());
        granted(&poll(
            &g,
            &GovernorPolicy::default(),
            t,
            AdmissionClass::Build,
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn snapshot_reports_margins_and_queue() {
        let (w, g) = World::new();
        let p = GovernorPolicy::default();
        w.set(|s| s.load1 = 55.0);
        queued(&g.acquire(&p, &req(AdmissionClass::Lander, 3)).unwrap());
        let snap = g.snapshot(&p);
        let load = snap.margins.iter().find(|m| m.gate == "load1").unwrap();
        assert!(!load.open);
        assert_eq!(load.threshold, 40.0);
        assert_eq!(snap.queue.len(), 1);
        assert_eq!(snap.queue[0].class, AdmissionClass::Lander);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn proc_stat_parsing_survives_odd_command_names() {
        let stat = "123 (my (weird) cmd) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 20";
        assert_eq!(parse_start_ticks(stat), Some(424_242));
        let me = resolve_proc_holder(std::process::id()).expect("own pid resolves");
        assert_eq!(resolve_proc_holder(std::process::id()), Some(me));
        assert!(resolve_proc_holder(u32::MAX - 1).is_none());
    }

    // ---- scripts/cargo-slot against a fake daemon socket ----

    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    fn script_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/cargo-slot")
    }

    fn tools_available() -> bool {
        ["ionice", "flock", "python3"].iter().all(|tool| {
            Command::new("sh")
                .args(["-c", &format!("command -v {tool}")])
                .stdout(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
    }

    /// Serve `AcquireAdmission`/`ReleaseAdmission` on `socket` with `governor`,
    /// replying with the same result shapes the RPC handlers produce.
    fn serve(socket: &std::path::Path, governor: Arc<Governor>, policy: GovernorPolicy) {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let governor = governor.clone();
                std::thread::spawn(move || {
                    let mut line = String::new();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let params = request["params"].clone();
                    let reply = match request["method"].as_str() {
                        Some("AcquireAdmission") => {
                            let params: AcquireParams = serde_json::from_value(params).unwrap();
                            match governor.acquire(&policy, &params) {
                                Ok(outcome) => serde_json::json!({ "result": outcome }),
                                Err(message) => serde_json::json!({
                                    "error": { "code": -32602, "message": message }
                                }),
                            }
                        }
                        Some("ReleaseAdmission") => {
                            let params: ReleaseParams = serde_json::from_value(params).unwrap();
                            serde_json::json!({
                                "result": { "released": governor.release(&policy, params.lease_id) }
                            })
                        }
                        _ => serde_json::json!({ "error": { "code": -32601, "message": "no" } }),
                    };
                    let mut stream = stream;
                    let _ = writeln!(stream, "{reply}");
                });
            }
        });
    }

    fn script(home: &std::path::Path, socket: &std::path::Path, args: &[&str]) -> Command {
        let mut command = Command::new(script_path());
        command
            .args(args)
            .env("HOME", home)
            .env("RSI_DAEMON_SOCKET_PATH", socket)
            .env("RSI_CARGO_MIN_FREE_GB", "0")
            .env("RSI_CARGO_MAX_LOAD", "100000")
            .env("RSI_CARGO_MIN_AVAIL_GB", "0")
            .env("RSI_CARGO_MAX_SLICE_GB", "100000")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn script_falls_back_to_local_gates_when_the_daemon_is_down() {
        if !tools_available() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".rsi")).unwrap();
        let missing = home.path().join("no-such.sock");
        let out = script(
            home.path(),
            &missing,
            &["sh", "-c", "echo jobs=$CARGO_BUILD_JOBS; exit 7"],
        )
        .output()
        .unwrap();
        assert_eq!(out.status.code(), Some(7), "exit status passes through");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "jobs=4");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn script_queues_behind_the_daemon_and_a_killed_client_frees_its_slot() {
        if !tools_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("d.sock");
        let governor = Arc::new(Governor::new(
            Box::new(healthy),
            Box::new(resolve_proc_holder),
        ));
        let policy = GovernorPolicy {
            build_slots: 1,
            ..GovernorPolicy::default()
        };
        serve(&socket, governor.clone(), policy);
        let wait_for = |what: &str, ok: &dyn Fn(&GovernorSnapshot) -> bool| {
            for _ in 0..100 {
                if ok(&governor.snapshot(&policy)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("timed out waiting for {what}");
        };

        let mut first = script(dir.path(), &socket, &["sleep", "61.37"])
            .spawn()
            .unwrap();
        wait_for("first lease", &|s| s.build_in_use == 1);
        let second = script(dir.path(), &socket, &["sh", "-c", "echo ran"])
            .spawn()
            .unwrap();
        wait_for("second queued", &|s| s.queue.len() == 1);
        let snap = governor.snapshot(&policy);
        assert_eq!(snap.queue[0].position, 0);
        // The wrapper is killed without cleanup; the daemon reaps its lease.
        let first_pid = first.id();
        let orphan = Command::new("pgrep")
            .args(["-P", &first_pid.to_string()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let _ = Command::new("kill")
            .args(["-9", &first_pid.to_string()])
            .status();
        let _ = first.wait();
        let out = second.wait_with_output().unwrap();
        assert!(out.status.success(), "queued run proceeds: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ran");
        // Cleanup the orphaned `sleep 61.37` child of the killed wrapper.
        if !orphan.is_empty() {
            let _ = Command::new("kill").args(["-9", &orphan]).status();
        }
        wait_for("all slots released", &|s| s.build_in_use == 0);
    }
}
