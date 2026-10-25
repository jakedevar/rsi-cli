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
//! The gates apply cargo-slot policy:
//! * disk: physical free on `/` must always meet `min_free_disk_gb`. With
//!   running growth reservations, free must also meet the floor after reserving
//!   those leases' and the newcomer's remaining target growth. Verified reuse
//!   reduces only the newcomer's growth. Without running reservations, a lone
//!   newcomer does not project its own growth, preserving cold-build liveness;
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
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const GIB: f64 = 1_073_741_824.0;

mod disk_growth;
pub(crate) mod own_target;

/// A queued (not yet granted) ticket that has not been polled for this long is
/// dropped: its client is gone or wedged.
pub const WAITER_STALE: Duration = Duration::from_secs(120);

/// Admission class. Landers (`rsi-rolling-land` runs), builds (everything
/// else) and checks (`cargo check` jobs, #2430) draw on separate slot pools so
/// landers cannot starve builds and a worker's compile-feedback run is never
/// queued behind full builds and test runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionClass {
    Build,
    Lander,
    /// The fast compile-feedback lane (#2430): `cargo check` only, its own
    /// pool and FIFO, and exempt from the load gate (disk and memory still apply).
    Check,
}

impl AdmissionClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Lander => "lander",
            Self::Check => "check",
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
    /// Free swap in GiB; counted as headroom by the projected memory gate
    /// (hub #3629). `0.0` where there is no swap or no source.
    #[serde(default)]
    pub swap_free_gb: f64,
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
    DiskGrowth {
        free_gb: f64,
        projected_free_gb: f64,
        min_gb: u64,
        #[serde(default)]
        requested_growth_gb: f64,
        holders: Vec<DiskGrowthHolder>,
    },
    Load {
        load1: f64,
        max: u32,
    },
    MemoryAvailable {
        available_gb: f64,
        min_gb: u64,
    },
    /// Memory headroom (`MemAvailable` + free swap) minus what recently started
    /// heavy builds are still expected to claim, and this request's own heavy
    /// reserve, is below `min_avail_mem_gb` (hub #3629).
    MemoryProjected {
        headroom_gb: f64,
        projected_gb: f64,
        min_gb: u64,
        reserved_gb: f64,
        requested_gb: f64,
    },
    WorkersSlice {
        used_gb: f64,
        max_gb: u64,
    },
    Slots {
        class: AdmissionClass,
        in_use: u32,
        cap: u32,
        /// Who holds the class's leases (#618): a waiter can tell a busy pool
        /// from a wedged holder. Empty from a peer that predates the field.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        holders: Vec<SlotHolder>,
    },
    QueueAhead {
        class: AdmissionClass,
        ahead: u32,
    },
}

/// A queued waiter the host disk floor alone holds (#625).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskStalledWaiter {
    /// The `rsi-job-<id>.service` unit the waiting client runs in, when `/proc`
    /// names one (Linux); a waiter outside a job unit has none.
    pub unit: Option<String>,
    /// Seconds the floor has held it continuously.
    pub stalled_secs: u64,
}

/// A queued waiter the host disk floor holds now (#582).
#[derive(Debug, Clone, PartialEq)]
pub struct DiskHeldWaiter {
    /// The `rsi-job-<id>.service` unit the waiting client runs in, when `/proc`
    /// names one.
    pub unit: Option<String>,
    /// When the floor first held this waiter (#644): the identity of one hold.
    pub held_since: DateTime<Utc>,
    /// Free GiB on the build filesystem at the governor sample.
    pub free_gb: f64,
    /// The configured floor in GiB.
    pub min_gb: u64,
}

/// One lease holding a class slot, as a waiter sees it (#618).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotHolder {
    pub lease_id: Uuid,
    pub pid: u32,
    pub label: Option<String>,
    /// Seconds since the lease was granted.
    pub held_secs: u64,
    /// The systemd unit the holder runs in (`rsi-job-<job id>.service`), when
    /// `/proc` names one: it maps the lease to a job and its owner session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

impl SlotHolder {
    fn describe(&self) -> String {
        format!(
            "{} pid {} ({}{}) for {}s",
            self.lease_id,
            self.pid,
            self.label.as_deref().unwrap_or("cargo unit"),
            self.unit
                .as_deref()
                .map_or_else(String::new, |unit| format!(", unit {unit}")),
            self.held_secs
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskGrowthHolder {
    pub lease_id: Uuid,
    pub pid: u32,
    pub label: Option<String>,
    pub remaining_gb: f64,
}

impl BlockReason {
    /// One-line operator text, also printed by the client while it waits.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Disk { free_gb, min_gb } => {
                format!(
                    "only {}; waiting for space",
                    crate::host_pressure::disk_floor_detail(*free_gb, *min_gb)
                )
            }
            Self::DiskGrowth {
                free_gb,
                projected_free_gb,
                min_gb,
                requested_growth_gb,
                holders,
            } => {
                let holders: Vec<String> = holders
                    .iter()
                    .map(|holder| {
                        format!(
                            "{} pid {} ({}) reserves {:.1} GiB",
                            holder.lease_id,
                            holder.pid,
                            holder.label.as_deref().unwrap_or("cargo unit"),
                            holder.remaining_gb
                        )
                    })
                    .collect();
                format!(
                    "projected free on / {projected_free_gb:.1} GiB (currently {free_gb:.1} GiB), floor {min_gb} GiB; requested growth {requested_growth_gb:.1} GiB; running reservations [{}]; waiting for space",
                    holders.join(", ")
                )
            }
            Self::Load { load1, max } => format!("load {load1:.1} >= {max}; waiting"),
            Self::MemoryAvailable {
                available_gb,
                min_gb,
            } => {
                format!("memory low (available {available_gb:.0} GB < {min_gb}); waiting")
            }
            Self::MemoryProjected {
                headroom_gb,
                projected_gb,
                min_gb,
                reserved_gb,
                requested_gb,
            } => format!(
                "projected memory headroom {projected_gb:.1} GiB (MemAvailable + free swap {headroom_gb:.1} GiB, floor {min_gb} GiB; running heavy builds reserve {reserved_gb:.1} GiB, this request {requested_gb:.1} GiB); waiting"
            ),
            Self::WorkersSlice { used_gb, max_gb } => {
                format!("workers slice uses {used_gb:.0} GB anon+shmem (>= {max_gb}); waiting")
            }
            Self::Slots {
                class,
                in_use,
                cap,
                holders,
            } => {
                let busy = format!("all {cap} {} slots busy ({in_use} in use)", class.as_str());
                if holders.is_empty() {
                    return busy;
                }
                let holders: Vec<String> = holders.iter().map(SlotHolder::describe).collect();
                format!("{busy}; held by {}", holders.join(", "))
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
    /// `/proc/<pid>/stat` field 22 (macOS: process start time in microseconds),
    /// guarding against pid reuse.
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
    /// Start of the current disk-floor hold, excluding earlier slot/load waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_wait_started_at: Option<DateTime<Utc>>,
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
    /// Leases of the fast `cargo check` lane (#2430).
    #[serde(default)]
    pub check_in_use: u32,
    pub leases: Vec<LeaseView>,
    pub queue: Vec<QueueEntryView>,
}

struct Waiter {
    ticket: Uuid,
    class: AdmissionClass,
    holder: Holder,
    label: Option<String>,
    enqueued_at: DateTime<Utc>,
    disk_wait_started_at: Option<DateTime<Utc>>,
    last_polled: Instant,
    disk_credit_gb: f64,
    growth: disk_growth::Usage,
}

struct Lease {
    id: Uuid,
    class: AdmissionClass,
    holder: Holder,
    label: Option<String>,
    granted_at: DateTime<Utc>,
    growth: disk_growth::Usage,
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
    sample_last_updated: Option<Arc<Mutex<Option<Instant>>>>,
    /// Current identity of a pid, or `None` when it is gone. A lease is live
    /// only while this equals the identity recorded at acquire time.
    resolve_holder: HolderFn,
    growth_probe: Box<dyn Fn(Holder) -> disk_growth::Usage + Send + Sync>,
    /// A disk-floor stall was already logged this episode (#932).
    disk_warned: std::sync::atomic::AtomicBool,
}

impl Governor {
    #[must_use]
    pub fn new(sample: SampleFn, resolve_holder: HolderFn) -> Self {
        Self {
            state: Mutex::new(State::default()),
            sample,
            sample_last_updated: None,
            resolve_holder,
            growth_probe: Box::new(|_| disk_growth::Usage::default()),
            disk_warned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The process-wide governor with real `/proc` and cgroup probes.
    #[must_use]
    pub fn global() -> &'static Self {
        static GLOBAL: LazyLock<Governor> =
            LazyLock::new(|| Governor::new_cached_host(Box::new(resolve_proc_holder)));
        &GLOBAL
    }

    fn new_cached_host(resolve_holder: HolderFn) -> Self {
        let growth = Arc::new(disk_growth::Probe::default());
        let refresh_growth = Arc::clone(&growth);
        let mut governor = Self::new_cached(
            Box::new(move || {
                refresh_growth.refresh();
                sample_host()
            }),
            resolve_holder,
            Duration::from_millis(500),
        );
        governor.growth_probe = Box::new(move |holder| growth.usage(holder));
        governor
    }

    fn new_cached(
        sample_fn: SampleFn,
        resolve_holder: HolderFn,
        sample_interval: Duration,
    ) -> Self {
        let sample = Arc::new(Mutex::new(ResourceSample {
            load1: f64::MAX,
            cores: 1,
            disk_free_gb: 0.0,
            mem_available_gb: 0.0,
            swap_free_gb: 0.0,
            workers_slice_anon_gb: None,
        }));
        let refresh_sample = Arc::clone(&sample);
        let sample_last_updated = Arc::new(Mutex::new(None));
        let refresh_last_updated = Arc::clone(&sample_last_updated);
        if let Err(error) = std::thread::Builder::new()
            .name("rsid-resource-sampler".into())
            .spawn(move || {
                loop {
                    // Host probes can block on filesystem or cgroup I/O under
                    // pressure. Keep them off Tokio RPC workers; callers read the
                    // last completed sample while this thread is stalled.
                    let current = sample_fn();
                    *refresh_sample
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = current;
                    *refresh_last_updated
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
                    std::thread::sleep(sample_interval);
                }
            })
        {
            tracing::error!(error = %error, "Failed to start resource sampler thread");
        }

        let mut governor = Self::new(
            Box::new(move || {
                sample
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            }),
            resolve_holder,
        );
        governor.sample_last_updated = Some(sample_last_updated);
        governor
    }

    fn sample_is_fresh(&self) -> bool {
        const MAX_SAMPLE_AGE: Duration = Duration::from_secs(5);
        let Some(last_updated) = &self.sample_last_updated else {
            return true;
        };
        last_updated
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|sampled_at| sampled_at.elapsed() <= MAX_SAMPLE_AGE)
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
        self.acquire_with_disk_credit(policy, params, 0.0)
    }

    /// The caller has verified a running job's own target. Reuse reduces only
    /// this ticket's growth estimate; physical free space and other reservations
    /// always retain their full values.
    pub(crate) fn acquire_with_disk_credit(
        &self,
        policy: &GovernorPolicy,
        params: &AcquireParams,
        disk_credit_gb: f64,
    ) -> Result<AcquireOutcome, String> {
        let disk_credit_gb = if disk_credit_gb.is_finite() {
            disk_credit_gb.clamp(0.0, 2.0_f64.min(policy.min_free_disk_gb as f64 / 2.0))
        } else {
            0.0
        };
        let sample = (self.sample)();
        let sample_is_fresh = self.sample_is_fresh();
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
                if waiter.holder.pid != params.pid && disk_credit_gb > 0.0 {
                    return Err("ticket holder does not match credited job pid".into());
                }
                waiter.last_polled = Instant::now();
                waiter.disk_credit_gb = disk_credit_gb;
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
                disk_wait_started_at: None,
                last_polled: Instant::now(),
                disk_credit_gb,
                growth: (self.growth_probe)(holder),
            });
        }
        let ticket = params.ticket_id.unwrap_or_else(|| {
            state
                .queue
                .back()
                .map_or_else(Uuid::nil, |waiter| waiter.ticket)
        });
        self.admit(&mut state, policy, &sample, sample_is_fresh);
        if let Some(lease) = state.leases.get(&ticket) {
            return Ok(AcquireOutcome::Granted {
                lease_id: lease.id,
                class: lease.class,
            });
        }
        let (class, position) = Self::position_of(&state, ticket)
            .ok_or_else(|| "ticket vanished during admission".to_string())?;
        let reason = Self::reason_for(
            &state,
            policy,
            &sample,
            ticket,
            class,
            position,
            sample_is_fresh,
        );
        self.note_disk_stall(policy, &sample, &reason);
        Ok(AcquireOutcome::Queued {
            ticket_id: ticket,
            class,
            position,
            message: format!(
                "{}{}",
                reason.describe(),
                holders_note(&state, class_of_slot_wait(&reason))
            ),
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
        if matches!(
            reason,
            BlockReason::Disk { .. } | BlockReason::DiskGrowth { .. }
        ) && !self.disk_warned.swap(true, Ordering::Relaxed)
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
        let sample_is_fresh = self.sample_is_fresh();
        let mut state = self.state();
        let mut existed = state.leases.remove(&lease_id).is_some();
        let before = state.queue.len();
        state.queue.retain(|w| w.ticket != lease_id);
        existed |= state.queue.len() != before;
        self.reap(&mut state);
        self.admit(&mut state, policy, &sample, sample_is_fresh);
        existed
    }

    /// #625: the queued waiters held only by the host disk floor, with how long
    /// each has been held. Empty while any lease holds a slot: a holder can still
    /// finish and free space, so that wait stays the long one. With no holder
    /// nothing frees the floor but the host, so a caller may settle such a wait
    /// early. Reads the live state without sampling or admitting.
    pub fn disk_stalled_waiters(&self) -> Vec<DiskStalledWaiter> {
        let mut state = self.state();
        self.reap(&mut state);
        if !state.leases.is_empty() {
            return Vec::new();
        }
        let now = Utc::now();
        state
            .queue
            .iter()
            .filter_map(|w| {
                let since = w.disk_wait_started_at?;
                Some(DiskStalledWaiter {
                    unit: job_unit_of_pid(w.holder.pid),
                    stalled_secs: u64::try_from((now - since).num_seconds()).unwrap_or(0),
                })
            })
            .collect()
    }

    /// #582: every queued waiter the host disk floor holds right now, with the
    /// sampled free space and the floor, so a job's owner can read why it waits.
    /// Unlike [`Self::disk_stalled_waiters`] a slot holder does not hide it: the
    /// wait is reported whenever the floor is what blocks the waiter.
    pub fn disk_held_waiters(&self, policy: &GovernorPolicy) -> Vec<DiskHeldWaiter> {
        let snapshot = self.snapshot(policy);
        snapshot
            .queue
            .iter()
            .filter_map(|entry| {
                let held_since = entry.disk_wait_started_at?;
                Some(DiskHeldWaiter {
                    unit: job_unit_of_pid(entry.pid),
                    held_since,
                    free_gb: snapshot.sample.disk_free_gb,
                    min_gb: policy.min_free_disk_gb,
                })
            })
            .collect()
    }

    /// Current policy margins, leases and queue.
    pub fn snapshot(&self, policy: &GovernorPolicy) -> GovernorSnapshot {
        let sample = (self.sample)();
        let sample_is_fresh = self.sample_is_fresh();
        let mut state = self.state();
        self.reap(&mut state);
        self.admit(&mut state, policy, &sample, sample_is_fresh);
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
                    disk_wait_started_at: w.disk_wait_started_at,
                }
            })
            .collect();
        GovernorSnapshot {
            policy: *policy,
            margins: margins(policy, &sample),
            sample,
            build_in_use: count(AdmissionClass::Build),
            lander_in_use: count(AdmissionClass::Lander),
            check_in_use: count(AdmissionClass::Check),
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
    fn admit(
        &self,
        state: &mut State,
        policy: &GovernorPolicy,
        sample: &ResourceSample,
        sample_is_fresh: bool,
    ) {
        for lease in state.leases.values_mut() {
            lease.growth = (self.growth_probe)(lease.holder);
        }
        for waiter in &mut state.queue {
            waiter.growth = (self.growth_probe)(waiter.holder);
        }
        let now = Utc::now();
        let growth = growth_holders(state);
        let mem_reserved_gb = heavy_mem_reserved_gb(state);
        for waiter in &mut state.queue {
            let disk_held = sample_is_fresh
                && matches!(
                    waiter_block(policy, sample, waiter, &growth, mem_reserved_gb),
                    Some(BlockReason::Disk { .. } | BlockReason::DiskGrowth { .. })
                );
            if disk_held {
                waiter.disk_wait_started_at.get_or_insert(now);
            } else {
                waiter.disk_wait_started_at = None;
            }
        }
        let mut blocked: Vec<AdmissionClass> = Vec::new();
        let mut index = 0;
        while index < state.queue.len() {
            let class = state.queue[index].class;
            if blocked.contains(&class) {
                index += 1;
                continue;
            }
            if sample_is_fresh {
                if let Some(reason) = waiter_block(
                    policy,
                    sample,
                    &state.queue[index],
                    &growth_holders(state),
                    heavy_mem_reserved_gb(state),
                ) {
                    if matches!(
                        reason,
                        BlockReason::Disk { .. } | BlockReason::DiskGrowth { .. }
                    ) {
                        state.queue[index].disk_wait_started_at.get_or_insert(now);
                    }
                    blocked.push(class);
                    index += 1;
                    continue;
                }
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
                        growth: (self.growth_probe)(waiter.holder),
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
        ticket: Uuid,
        class: AdmissionClass,
        position: u32,
        sample_is_fresh: bool,
    ) -> BlockReason {
        if sample_is_fresh
            && let Some(waiter) = state.queue.iter().find(|waiter| waiter.ticket == ticket)
            && let Some(reason) = waiter_block(
                policy,
                sample,
                waiter,
                &growth_holders(state),
                heavy_mem_reserved_gb(state),
            )
        {
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
            holders: slot_holders(state, class),
        }
    }
}

fn waiter_block(
    policy: &GovernorPolicy,
    sample: &ResourceSample,
    waiter: &Waiter,
    growth: &[DiskGrowthHolder],
    mem_reserved_gb: f64,
) -> Option<BlockReason> {
    // A snapshot/release may admit another ticket. Never reuse old filesystem
    // evidence indefinitely when its caller has stopped polling.
    let credit = if waiter.last_polled.elapsed() <= Duration::from_secs(5) {
        waiter.disk_credit_gb
    } else {
        0.0
    };
    // The physical floor is independent of reuse and estimated future growth.
    let physical = global_block(policy, sample, waiter.class);
    if matches!(physical, Some(BlockReason::Disk { .. })) {
        return physical;
    }
    if physical.is_none()
        && let Some(projected) = memory_projection_block(policy, sample, waiter, mem_reserved_gb)
    {
        return Some(projected);
    }
    let reserved_gb: f64 = growth.iter().map(|holder| holder.remaining_gb).sum();
    // Preserve lone-build liveness above the hard physical floor. Project the
    // requester's growth only when protecting another lease's reservation.
    if reserved_gb <= 0.0 {
        return physical;
    }
    // The background probe may already have deducted this target's allocated
    // bytes. Use whichever evidence shows more reuse, never count it twice.
    let measured_reuse_gb = waiter.growth.current_bytes as f64 / GIB;
    let additional_reuse_gb = (credit - measured_reuse_gb).max(0.0);
    let requested_growth_gb = (waiter.growth.remaining_gb() - additional_reuse_gb).max(0.0);
    let projected_free_gb = sample.disk_free_gb - reserved_gb - requested_growth_gb;
    if projected_free_gb < policy.min_free_disk_gb as f64 {
        return Some(BlockReason::DiskGrowth {
            free_gb: sample.disk_free_gb,
            projected_free_gb,
            min_gb: policy.min_free_disk_gb,
            requested_growth_gb,
            holders: growth.to_vec(),
        });
    }
    physical
}

fn growth_holders(state: &State) -> Vec<DiskGrowthHolder> {
    let mut holders: Vec<_> = state
        .leases
        .values()
        .filter_map(|lease| {
            let remaining_gb = lease.growth.remaining_gb();
            (remaining_gb > 0.0).then(|| DiskGrowthHolder {
                lease_id: lease.id,
                pid: lease.holder.pid,
                label: lease.label.clone(),
                remaining_gb,
            })
        })
        .collect();
    holders.sort_by_key(|holder| holder.lease_id);
    holders
}

/// Memory a freshly started heavy build (an `rsid` test or workspace compile)
/// is expected to claim beyond what `MemAvailable` shows at admission: the
/// hub saw four concurrent `rsid --test` compiles at 3.7-10.3 GB RSS each
/// (hub #3629). A constant: the operator tunes the floor it is projected
/// against (`governor_min_avail_mem_gb`), not the estimate.
const HEAVY_BUILD_RESERVE_GB: f64 = 8.0;

/// A granted heavy build's reserve decays linearly to zero over this long, by
/// which time its real usage is in `MemAvailable`.
const HEAVY_BUILD_RAMP_SECS: f64 = 600.0;

/// Whether a cargo command line compiles the big `rsid` test binary or the
/// whole workspace. Matches `-p rsid`, `--package rsid` (also `=` forms) or
/// `--workspace` together with a test-building word, and the rustc form
/// `--crate-name rsid ... --test`. The fast `check` lane is never heavy.
fn is_heavy_build(class: AdmissionClass, label: Option<&str>) -> bool {
    let Some(label) = label else {
        return false;
    };
    if class == AdmissionClass::Check {
        return false;
    }
    let tokens: Vec<&str> = label.split_whitespace().collect();
    let names_package = tokens
        .windows(2)
        .any(|pair| matches!(pair[0], "-p" | "--package" | "--crate-name") && pair[1] == "rsid")
        || tokens
            .iter()
            .any(|token| matches!(*token, "-p=rsid" | "--package=rsid" | "--crate-name=rsid"));
    let wide = tokens.contains(&"--workspace");
    let builds_tests = tokens.iter().any(|token| {
        matches!(
            *token,
            "test" | "nextest" | "--tests" | "--test" | "--all-targets" | "--no-run"
        )
    });
    (names_package || wide) && builds_tests
}

/// Memory still owed by granted heavy builds: each reserves
/// [`HEAVY_BUILD_RESERVE_GB`] at grant, decaying over [`HEAVY_BUILD_RAMP_SECS`].
fn heavy_mem_reserved_gb(state: &State) -> f64 {
    let now = Utc::now();
    state
        .leases
        .values()
        .filter(|lease| is_heavy_build(lease.class, lease.label.as_deref()))
        .map(|lease| {
            let age = (now - lease.granted_at).num_milliseconds() as f64 / 1000.0;
            HEAVY_BUILD_RESERVE_GB * (1.0 - age / HEAVY_BUILD_RAMP_SECS).clamp(0.0, 1.0)
        })
        .sum()
}

/// The projected memory gate: `MemAvailable` plus free swap, less the reserve
/// of running heavy builds and of this request when it is heavy, must still
/// meet `min_avail_mem_gb`. Silent (`None`) when nothing is reserved, so the
/// plain physical floor in [`global_block`] stays the only memory rule for
/// light work and a lone light job is never held by a projection.
fn memory_projection_block(
    policy: &GovernorPolicy,
    sample: &ResourceSample,
    waiter: &Waiter,
    reserved_gb: f64,
) -> Option<BlockReason> {
    let requested_gb = if is_heavy_build(waiter.class, waiter.label.as_deref()) {
        HEAVY_BUILD_RESERVE_GB
    } else {
        0.0
    };
    if reserved_gb + requested_gb <= 0.0 {
        return None;
    }
    let headroom_gb = sample.mem_available_gb + sample.swap_free_gb.max(0.0);
    let projected_gb = headroom_gb - reserved_gb - requested_gb;
    if projected_gb >= policy.min_avail_mem_gb as f64 {
        return None;
    }
    Some(BlockReason::MemoryProjected {
        headroom_gb,
        projected_gb,
        min_gb: policy.min_avail_mem_gb,
        reserved_gb,
        requested_gb,
    })
}

/// Free swap in GiB from `/proc/meminfo`; `0.0` when unreadable or absent.
#[cfg(target_os = "linux")]
pub(crate) fn read_swap_free_gb() -> f64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|body| meminfo_kib(&body, "SwapFree:"))
        .map_or(0.0, |kib| kib as f64 * 1024.0 / GIB)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_swap_free_gb() -> f64 {
    0.0
}

/// The class whose lease holders explain a slot or queue wait (#1591).
fn class_of_slot_wait(reason: &BlockReason) -> Option<AdmissionClass> {
    match reason {
        BlockReason::Slots { class, .. } | BlockReason::QueueAhead { class, .. } => Some(*class),
        _ => None,
    }
}

/// `; held by pid N (label), ...`: who holds the class's slots, so a waiting
/// job's log names the holder (#1591). Empty when the wait is not slot-bound.
fn holders_note(state: &State, class: Option<AdmissionClass>) -> String {
    let Some(class) = class else {
        return String::new();
    };
    let mut held: Vec<&Lease> = state.leases.values().filter(|l| l.class == class).collect();
    held.sort_by_key(|lease| lease.granted_at);
    if held.is_empty() {
        return String::new();
    }
    let shown: Vec<String> = held
        .iter()
        .take(3)
        .map(|lease| {
            let label: String = lease
                .label
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            format!("pid {} ({label})", lease.holder.pid)
        })
        .collect();
    let more = held.len().saturating_sub(shown.len());
    let tail = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    format!("; held by {}{tail}", shown.join(", "))
}

/// The leases holding `class` slots, longest-held first.
fn slot_holders(state: &State, class: AdmissionClass) -> Vec<SlotHolder> {
    let now = Utc::now();
    let mut holders: Vec<SlotHolder> = state
        .leases
        .values()
        .filter(|l| l.class == class)
        .map(|l| SlotHolder {
            lease_id: l.id,
            pid: l.holder.pid,
            label: l.label.clone(),
            held_secs: u64::try_from((now - l.granted_at).num_seconds()).unwrap_or(0),
            unit: job_unit_of_pid(l.holder.pid),
        })
        .collect();
    holders.sort_by(|a, b| {
        b.held_secs
            .cmp(&a.held_secs)
            .then(a.lease_id.cmp(&b.lease_id))
    });
    holders
}

/// The `rsi-job-<id>.service` unit a process runs in, from its cgroup.
#[cfg(target_os = "linux")]
fn job_unit_of_pid(pid: u32) -> Option<String> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    unit_in_cgroup(&cgroup)
}

#[cfg(not(target_os = "linux"))]
fn job_unit_of_pid(_pid: u32) -> Option<String> {
    None
}

/// The innermost `rsi-job-*.service` path segment in `/proc/<pid>/cgroup` text.
#[cfg(any(target_os = "linux", test))]
fn unit_in_cgroup(cgroup: &str) -> Option<String> {
    cgroup
        .lines()
        .flat_map(|line| line.rsplit('/'))
        .find(|segment| segment.starts_with("rsi-job-") && segment.ends_with(".service"))
        .map(str::to_string)
}

fn class_in_use(state: &State, class: AdmissionClass) -> u32 {
    u32::try_from(state.leases.values().filter(|l| l.class == class).count()).unwrap_or(u32::MAX)
}

fn class_cap(policy: &GovernorPolicy, class: AdmissionClass) -> u32 {
    match class {
        AdmissionClass::Build => policy.build_slots,
        AdmissionClass::Lander => policy.lander_slots,
        AdmissionClass::Check => policy.check_slots,
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

/// First closed host gate for `class`, in cargo-slot order: disk, load, memory,
/// slice. The check lane (#2430) skips the load gate: its whole purpose is
/// feedback while full builds saturate the host, and the build slice's CPU cap
/// still bounds it; disk and memory floors apply to every class.
fn global_block(
    policy: &GovernorPolicy,
    sample: &ResourceSample,
    class: AdmissionClass,
) -> Option<BlockReason> {
    if crate::host_pressure::disk_below_floor(sample.disk_free_gb, policy.min_free_disk_gb) {
        return Some(BlockReason::Disk {
            free_gb: sample.disk_free_gb,
            min_gb: policy.min_free_disk_gb,
        });
    }
    let max_load = effective_max_load(policy, sample.cores);
    if class != AdmissionClass::Check && sample.load1 >= f64::from(max_load) {
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
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn resolve_proc_holder(pid: u32) -> Option<Holder> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let start_ticks = parse_start_ticks(&stat)?;
    Some(Holder { pid, start_ticks })
}

/// Identity of `pid` from `proc_pidinfo(PROC_PIDTBSDINFO)`, or `None` when it
/// is gone. macOS has no `/proc`; the process start time (microseconds since
/// the epoch) plays the role of the Linux start ticks and guards against pid
/// reuse the same way.
#[cfg(target_os = "macos")]
#[must_use]
pub fn resolve_proc_holder(pid: u32) -> Option<Holder> {
    let mut info = std::mem::MaybeUninit::<nix::libc::proc_bsdinfo>::zeroed();
    let size = i32::try_from(std::mem::size_of::<nix::libc::proc_bsdinfo>()).ok()?;
    // SAFETY: the buffer is a properly sized and aligned `proc_bsdinfo` that
    // outlives the call; proc_pidinfo writes at most `size` bytes into it.
    let written = unsafe {
        nix::libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            nix::libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: proc_pidinfo filled the whole struct (checked above).
    let info = unsafe { info.assume_init() };
    let start_ticks = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)?;
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

/// One `/proc/meminfo` field in KiB (`key` includes the trailing colon).
#[cfg(target_os = "linux")]
fn meminfo_kib(meminfo: &str, key: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

#[cfg(target_os = "linux")]
fn mem_available_kib(meminfo: &str) -> Option<u64> {
    meminfo_kib(meminfo, "MemAvailable:")
}

/// Swap in use as a percentage of total swap from a `/proc/meminfo` body;
/// `None` when the host has no swap or the fields are missing.
#[cfg(target_os = "linux")]
pub(crate) fn swap_used_pct_of(meminfo: &str) -> Option<f64> {
    let total = meminfo_kib(meminfo, "SwapTotal:").filter(|total| *total > 0)?;
    let free = meminfo_kib(meminfo, "SwapFree:")?;
    Some(total.saturating_sub(free) as f64 * 100.0 / total as f64)
}

/// Swap use (percent) now; `None` where there is no swap or no source.
#[cfg(target_os = "linux")]
pub(crate) fn read_swap_used_pct() -> Option<f64> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|body| swap_used_pct_of(&body))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_swap_used_pct() -> Option<f64> {
    None
}

fn workers_slice_stat_path() -> std::path::PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/rsi.slice/rsi-workers.slice/memory.stat"
    )
    .into()
}

/// The 1-minute load: `/proc/loadavg` on Linux, `getloadavg(3)` elsewhere
/// (#2937). Unreadable counts as idle, as cargo-slot did.
#[cfg(target_os = "linux")]
fn read_load1() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|body| body.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}

#[cfg(not(target_os = "linux"))]
fn read_load1() -> f64 {
    crate::daemon_info::load_average().map_or(0.0, |load| load.one)
}

/// Available memory in GiB; `f64::MAX` (the gate passes) when unreadable.
#[cfg(target_os = "linux")]
pub(crate) fn read_mem_available_gb() -> f64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|body| mem_available_kib(&body))
        .map_or(f64::MAX, |kib| kib as f64 * 1024.0 / GIB)
}

/// macOS: `vm_stat` free + inactive + speculative pages (#2937).
#[cfg(not(target_os = "linux"))]
pub(crate) fn read_mem_available_gb() -> f64 {
    crate::host_memory::read_available_bytes().map_or(f64::MAX, |bytes| bytes as f64 / GIB)
}

fn sample_host() -> ResourceSample {
    let load1 = read_load1();
    let cores = std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1);
    let disk_free_gb = crate::host_pressure::disk_free_gb();
    let mem_available_gb = read_mem_available_gb();
    let workers_slice_anon_gb = std::fs::read_to_string(workers_slice_stat_path())
        .ok()
        .map(|body| anon_shmem_bytes(&body) as f64 / GIB);
    ResourceSample {
        load1,
        cores,
        disk_free_gb,
        mem_available_gb,
        swap_free_gb: read_swap_free_gb(),
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
            disk_free_gb: 200.0,
            mem_available_gb: 64.0,
            swap_free_gb: 0.0,
            workers_slice_anon_gb: Some(2.0),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn governor_snapshot_does_not_wait_for_host_resource_probe() {
        let governor = Governor::new_cached(
            Box::new(|| {
                std::thread::sleep(Duration::from_millis(500));
                healthy()
            }),
            Box::new(|pid| {
                Some(Holder {
                    pid,
                    start_ticks: 7,
                })
            }),
            Duration::from_millis(10),
        );

        let started = Instant::now();
        let outcome = governor
            .acquire(
                &GovernorPolicy::LINUX_DEFAULT,
                &req(AdmissionClass::Build, 7),
            )
            .expect("acquire while first host probe is in progress");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "governor admission waited for the host resource probe"
        );
        assert!(matches!(outcome, AcquireOutcome::Granted { .. }));
    }

    fn req(class: AdmissionClass, pid: u32) -> AcquireParams {
        AcquireParams {
            class,
            pid,
            label: None,
            ticket_id: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn heavy_req(pid: u32) -> AcquireParams {
        AcquireParams {
            label: Some("cargo test -p rsid --lib --no-run".into()),
            ..req(AdmissionClass::Build, pid)
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn repoll(ticket_id: Uuid, pid: u32) -> AcquireParams {
        AcquireParams {
            ticket_id: Some(ticket_id),
            ..req(AdmissionClass::Build, pid)
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn heavy_builds_are_recognised_from_the_cargo_command_line() {
        let heavy = |class, label: &str| is_heavy_build(class, Some(label));
        let build = AdmissionClass::Build;
        assert!(heavy(build, "cargo test -p rsid --lib"));
        assert!(heavy(build, "cargo test --package=rsid --no-run"));
        assert!(heavy(build, "cargo test --workspace"));
        assert!(heavy(build, "rustc --crate-name rsid --test"));
        assert!(!heavy(build, "cargo build -p rsid-store"));
        assert!(!heavy(build, "cargo test -p rsid-store --lib"));
        assert!(!heavy(AdmissionClass::Check, "cargo test -p rsid --lib"));
        assert!(!is_heavy_build(build, None));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn heavy_build_is_held_by_projected_memory_and_admitted_once_headroom_returns() {
        let (world, governor) = World::new();
        let policy = GovernorPolicy::LINUX_DEFAULT;
        world.set(|s| s.mem_available_gb = 30.0);
        assert!(matches!(
            governor.acquire(&policy, &heavy_req(1)).unwrap(),
            AcquireOutcome::Granted { .. }
        ));
        // 30 GiB available, 8 reserved by the running heavy build and 8 more
        // for this one leaves 14 GiB, below the 16 GiB floor: held.
        let AcquireOutcome::Queued {
            ticket_id, reason, ..
        } = governor.acquire(&policy, &heavy_req(2)).unwrap()
        else {
            panic!("second heavy build must be held below the projected floor");
        };
        let BlockReason::MemoryProjected {
            projected_gb,
            reserved_gb,
            requested_gb,
            min_gb,
            ..
        } = reason
        else {
            panic!("expected MemoryProjected, got {reason:?}");
        };
        assert_eq!(min_gb, policy.min_avail_mem_gb);
        assert!((requested_gb - HEAVY_BUILD_RESERVE_GB).abs() < f64::EPSILON);
        assert!(reserved_gb > 7.9 && reserved_gb <= HEAVY_BUILD_RESERVE_GB);
        assert!(projected_gb < policy.min_avail_mem_gb as f64);
        // More available memory admits it.
        world.set(|s| s.mem_available_gb = 40.0);
        assert!(matches!(
            governor.acquire(&policy, &repoll(ticket_id, 2)).unwrap(),
            AcquireOutcome::Granted { .. }
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn free_swap_counts_toward_the_projected_memory_headroom() {
        let (world, governor) = World::new();
        let policy = GovernorPolicy::LINUX_DEFAULT;
        world.set(|s| s.mem_available_gb = 30.0);
        governor.acquire(&policy, &heavy_req(1)).unwrap();
        let AcquireOutcome::Queued { ticket_id, .. } =
            governor.acquire(&policy, &heavy_req(2)).unwrap()
        else {
            panic!("held without swap");
        };
        world.set(|s| s.swap_free_gb = 10.0);
        assert!(matches!(
            governor.acquire(&policy, &repoll(ticket_id, 2)).unwrap(),
            AcquireOutcome::Granted { .. }
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_lone_light_job_is_never_held_by_the_projection_and_a_ramped_reserve_expires() {
        let (world, governor) = World::new();
        let policy = GovernorPolicy::LINUX_DEFAULT;
        // The plain floor still admits a light job when nothing is reserved.
        world.set(|s| s.mem_available_gb = 16.0);
        assert!(matches!(
            governor
                .acquire(&policy, &req(AdmissionClass::Build, 1))
                .unwrap(),
            AcquireOutcome::Granted { .. }
        ));
        world.set(|s| s.mem_available_gb = 30.0);
        governor.acquire(&policy, &heavy_req(2)).unwrap();
        let AcquireOutcome::Queued {
            ticket_id, reason, ..
        } = governor.acquire(&policy, &heavy_req(3)).unwrap()
        else {
            panic!("held while the first heavy build ramps up");
        };
        assert!(matches!(reason, BlockReason::MemoryProjected { .. }));
        // Once the first heavy build is past its ramp its reserve is gone.
        for lease in governor.state().leases.values_mut() {
            lease.granted_at = Utc::now() - chrono::Duration::seconds(700);
        }
        assert!(matches!(
            governor.acquire(&policy, &repoll(ticket_id, 3)).unwrap(),
            AcquireOutcome::Granted { .. }
        ));
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn swap_use_is_read_from_proc_meminfo() {
        let meminfo = "MemAvailable: 1024 kB\nSwapTotal: 1000 kB\nSwapFree: 150 kB\n";
        assert_eq!(meminfo_kib(meminfo, "SwapFree:"), Some(150));
        let used = swap_used_pct_of(meminfo).expect("swap present");
        assert!((used - 85.0).abs() < 1e-9);
        assert_eq!(swap_used_pct_of("SwapTotal: 0 kB\nSwapFree: 0 kB\n"), None);
        assert_eq!(swap_used_pct_of("MemAvailable: 1 kB\n"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn second_build_reserves_running_targets_remaining_growth() {
        let (world, mut governor) = World::new();
        governor.growth_probe = Box::new(|_| disk_growth::Usage {
            expected_bytes: 22 * 1_073_741_824,
            current_bytes: 4 * 1_073_741_824,
        });
        let policy = GovernorPolicy::LINUX_DEFAULT;
        world.set(|sample| sample.disk_free_gb = 65.0);
        granted(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 1))
                .unwrap(),
        );
        let (ticket, _, reason) = queued(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 2))
                .unwrap(),
        );
        assert!(matches!(
            reason,
            BlockReason::DiskGrowth {
                projected_free_gb: 29.0,
                ..
            }
        ));
        world.set(|sample| sample.disk_free_gb = 66.0);
        // Exactly at the projected floor is sufficient; the running unit keeps
        // its lease throughout the hold and the newcomer is never cancelled.
        granted(&poll(&governor, &policy, ticket, AdmissionClass::Build));
        assert_eq!(governor.snapshot(&policy).build_in_use, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn lone_cold_build_is_admitted_at_or_above_the_physical_floor() {
        for class in [
            AdmissionClass::Build,
            AdmissionClass::Check,
            AdmissionClass::Lander,
        ] {
            for free in [30.0, 30.1, 41.0] {
                let (world, mut governor) = World::new();
                governor.growth_probe = Box::new(|_| disk_growth::Usage {
                    expected_bytes: 96 * 1_073_741_824,
                    current_bytes: 0,
                });
                let policy = GovernorPolicy::LINUX_DEFAULT;
                world.set(|sample| sample.disk_free_gb = free);
                let lease = granted(&governor.acquire(&policy, &req(class, 1)).unwrap());
                assert_eq!(governor.snapshot(&policy).leases[0].lease_id, lease);
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn growth_hold_names_the_holder_and_projected_free_space() {
        let (world, governor) = World::new();
        let policy = GovernorPolicy::LINUX_DEFAULT;
        world.set(|sample| sample.disk_free_gb = 67.0);
        let holder = granted(
            &governor
                .acquire(
                    &policy,
                    &AcquireParams {
                        label: Some("scoped_test holder unit".into()),
                        ..req(AdmissionClass::Build, 1)
                    },
                )
                .unwrap(),
        );
        let outcome = governor
            .acquire(&policy, &req(AdmissionClass::Build, 2))
            .unwrap();
        let (ticket, _, reason) = queued(&outcome);
        let BlockReason::DiskGrowth {
            projected_free_gb,
            holders,
            ..
        } = reason
        else {
            panic!("expected a growth hold");
        };
        assert_eq!(projected_free_gb, 19.0);
        assert_eq!(holders[0].lease_id, holder);
        assert_eq!(holders[0].pid, 1);
        assert_eq!(holders[0].label.as_deref(), Some("scoped_test holder unit"));
        let AcquireOutcome::Queued { message, .. } = outcome else {
            panic!("expected queued")
        };
        assert!(message.contains("projected free on / 19.0 GiB"));
        assert!(message.contains(&holder.to_string()));
        assert!(message.contains("pid 1 (scoped_test holder unit)"));
        assert!(message.contains("requested growth 24.0 GiB"));
        assert!(
            governor.snapshot(&policy).queue[0]
                .disk_wait_started_at
                .is_some()
        );
        // Release, rather than cancellation, lets the queued unit run alone.
        assert!(governor.release(&policy, holder));
        granted(&poll(&governor, &policy, ticket, AdmissionClass::Build));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn unknown_growth_reserves_the_conservative_default_across_classes() {
        let (world, governor) = World::new();
        let policy = GovernorPolicy::LINUX_DEFAULT;
        world.set(|sample| sample.disk_free_gb = 77.0);
        granted(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 1))
                .unwrap(),
        );
        let (_, _, reason) = queued(
            &governor
                .acquire(&policy, &req(AdmissionClass::Check, 2))
                .unwrap(),
        );
        assert!(
            matches!(reason, BlockReason::DiskGrowth { projected_free_gb: 29.0, ref holders, .. } if holders[0].remaining_gb == 24.0)
        );
        world.set(|sample| sample.disk_free_gb = 78.0);
        assert_eq!(governor.snapshot(&policy).check_in_use, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn growth_projection_sums_every_running_cargo_lease() {
        let (world, mut governor) = World::new();
        governor.growth_probe = Box::new(|_| disk_growth::Usage {
            expected_bytes: 22 * 1_073_741_824,
            current_bytes: 4 * 1_073_741_824,
        });
        let policy = GovernorPolicy::LINUX_DEFAULT;
        world.set(|sample| sample.disk_free_gb = 83.0);
        granted(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 1))
                .unwrap(),
        );
        granted(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 2))
                .unwrap(),
        );
        let (_, _, reason) = queued(
            &governor
                .acquire(&policy, &req(AdmissionClass::Check, 3))
                .unwrap(),
        );
        assert!(
            matches!(reason, BlockReason::DiskGrowth { projected_free_gb: 29.0, ref holders, .. } if holders.len() == 2)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn admission_batch_charges_each_new_lease_before_granting_the_next() {
        let (world, governor) = World::new();
        let policy = GovernorPolicy::LINUX_DEFAULT;
        world.set(|sample| {
            sample.disk_free_gb = 67.0;
            sample.load1 = 1000.0;
        });
        let (first, _, _) = queued(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 1))
                .unwrap(),
        );
        let (second, _, _) = queued(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 2))
                .unwrap(),
        );
        world.set(|sample| sample.load1 = 1.0);
        let snapshot = governor.snapshot(&policy);
        assert_eq!(snapshot.leases[0].lease_id, first);
        assert_eq!(snapshot.queue[0].ticket_id, second);
        assert!(snapshot.queue[0].disk_wait_started_at.is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn own_target_reuse_never_admits_below_the_physical_floor() {
        for free in [28.3, (30.0 * GIB - 1.0) / GIB] {
            for credit in [0.0, 2.0] {
                let (world, governor) = World::new();
                let policy = GovernorPolicy::LINUX_DEFAULT;
                world.set(|sample| sample.disk_free_gb = free);
                // Even a caller supplying the maximum discount cannot change the
                // physical floor; the real RPC offers no reuse below it.
                let (_, _, reason) = queued(
                    &governor
                        .acquire_with_disk_credit(&policy, &req(AdmissionClass::Build, 1), credit)
                        .unwrap(),
                );
                assert_eq!(
                    reason,
                    BlockReason::Disk {
                        free_gb: free,
                        min_gb: 30
                    }
                );
                assert_eq!(
                    crate::host_pressure::own_target_credit_gb(free, 30, 8 * 1_073_741_824),
                    0.0
                );
                assert_eq!(governor.snapshot(&policy).build_in_use, 0);
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn own_target_reuse_lowers_only_the_requesting_jobs_growth() {
        let (world, governor) = World::new();
        let policy = GovernorPolicy::LINUX_DEFAULT;
        world.set(|sample| sample.disk_free_gb = 76.0);
        let holder = granted(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 1))
                .unwrap(),
        );
        let request = req(AdmissionClass::Build, 2);
        let (ticket, _, reason) = queued(&governor.acquire(&policy, &request).unwrap());
        assert!(matches!(reason, BlockReason::DiskGrowth {
            projected_free_gb: 28.0, requested_growth_gb: 24.0, ref holders, ..
        } if holders[0].lease_id == holder && holders[0].remaining_gb == 24.0));
        // Own-target reuse lowers the requested 24-GiB estimate to 22 GiB,
        // preserving the holder's full reservation and the 30-GiB floor.
        let credit = crate::host_pressure::own_target_credit_gb(76.0, 30, 8 * 1_073_741_824);
        assert_eq!(credit, 2.0);
        granted(
            &governor
                .acquire_with_disk_credit(
                    &policy,
                    &AcquireParams {
                        ticket_id: Some(ticket),
                        ..request
                    },
                    credit,
                )
                .unwrap(),
        );
        let (_, _, reason) = queued(
            &governor
                .acquire(&policy, &req(AdmissionClass::Check, 3))
                .unwrap(),
        );
        assert!(matches!(reason, BlockReason::DiskGrowth {
            requested_growth_gb: 24.0, ref holders, ..
        } if holders.len() == 2));
        let snapshot = governor.snapshot(&policy);
        assert_eq!(snapshot.sample.disk_free_gb, 76.0);
        assert_eq!(snapshot.build_in_use, 2);
        assert_eq!(snapshot.check_in_use, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn own_target_reuse_does_not_count_measured_target_bytes_twice() {
        let (world, mut governor) = World::new();
        governor.growth_probe = Box::new(|_| disk_growth::Usage {
            expected_bytes: 24 * 1_073_741_824,
            current_bytes: 8 * 1_073_741_824,
        });
        let policy = GovernorPolicy::LINUX_DEFAULT;
        granted(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 1))
                .unwrap(),
        );
        world.set(|sample| sample.disk_free_gb = 60.0);
        let (_, _, reason) = queued(
            &governor
                .acquire_with_disk_credit(&policy, &req(AdmissionClass::Build, 2), 2.0)
                .unwrap(),
        );
        assert!(matches!(
            reason,
            BlockReason::DiskGrowth {
                projected_free_gb: 28.0,
                requested_growth_gb: 16.0,
                ..
            }
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn own_target_reuse_at_the_floor_cannot_offset_another_lease() {
        let (world, mut governor) = World::new();
        // The requester needs one GiB; the running holder still needs 24 GiB.
        governor.growth_probe = Box::new(|holder| disk_growth::Usage {
            expected_bytes: if holder.pid == 1 { 24 } else { 1 } * 1_073_741_824,
            current_bytes: 0,
        });
        let policy = GovernorPolicy::LINUX_DEFAULT;
        granted(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 1))
                .unwrap(),
        );
        world.set(|sample| sample.disk_free_gb = 53.0);
        let request = req(AdmissionClass::Check, 2);
        let (ticket, _, reason) = queued(
            &governor
                .acquire_with_disk_credit(&policy, &request, 2.0)
                .unwrap(),
        );
        assert!(matches!(reason, BlockReason::DiskGrowth {
            projected_free_gb: 29.0, requested_growth_gb: 0.0, ref holders, ..
        } if holders[0].remaining_gb == 24.0));
        world.set(|sample| sample.disk_free_gb = 30.0);
        let request = AcquireParams {
            ticket_id: Some(ticket),
            ..request
        };
        queued(&governor.acquire(&policy, &request).unwrap());
        world.set(|sample| sample.disk_free_gb = 54.0);
        // With the holder still running, a cold estimate holds. Reuse saturates
        // at zero and permits exactly the projected floor without extra space.
        let (_, _, reason) = queued(&governor.acquire(&policy, &request).unwrap());
        assert!(matches!(
            reason,
            BlockReason::DiskGrowth {
                projected_free_gb: 29.0,
                ..
            }
        ));
        granted(
            &governor
                .acquire_with_disk_credit(&policy, &request, 2.0)
                .unwrap(),
        );
        assert_eq!(governor.snapshot(&policy).sample.disk_free_gb, 54.0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn own_target_reuse_keeps_memory_and_fifo_gates_and_expires() {
        let (world, governor) = World::new();
        let policy = GovernorPolicy::LINUX_DEFAULT;
        granted(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 1))
                .unwrap(),
        );
        world.set(|sample| {
            sample.disk_free_gb = 76.0;
            sample.mem_available_gb = 0.0;
        });
        let (ticket, _, reason) = queued(
            &governor
                .acquire_with_disk_credit(&policy, &req(AdmissionClass::Build, 2), 2.0)
                .unwrap(),
        );
        assert!(matches!(reason, BlockReason::MemoryAvailable { .. }));
        governor.state().queue[0].last_polled = Instant::now() - Duration::from_secs(6);
        world.set(|sample| sample.mem_available_gb = 64.0);
        assert_eq!(governor.snapshot(&policy).build_in_use, 1);
        // Expired evidence restores the full requested growth estimate.
        let (_, _, reason) = queued(
            &governor
                .acquire(
                    &policy,
                    &AcquireParams {
                        ticket_id: Some(ticket),
                        ..req(AdmissionClass::Build, 2)
                    },
                )
                .unwrap(),
        );
        assert!(matches!(
            reason,
            BlockReason::DiskGrowth {
                requested_growth_gb: 24.0,
                ..
            }
        ));
        let (_, position, _) = queued(
            &governor
                .acquire_with_disk_credit(&policy, &req(AdmissionClass::Build, 3), 2.0)
                .unwrap(),
        );
        assert_eq!(position, 1);
        assert_eq!(governor.snapshot(&policy).build_in_use, 1);
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
        let p = GovernorPolicy::LINUX_DEFAULT;
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
    fn a_slot_wait_message_names_the_holders() {
        let (_w, g) = World::new();
        let p = GovernorPolicy {
            build_slots: 1,
            ..GovernorPolicy::LINUX_DEFAULT
        };
        let mut holder = req(AdmissionClass::Build, 1);
        holder.label = Some("make scoped-test".into());
        granted(&g.acquire(&p, &holder).unwrap());
        match g.acquire(&p, &req(AdmissionClass::Build, 2)).unwrap() {
            AcquireOutcome::Queued { message, .. } => {
                assert!(
                    message.ends_with("; held by pid 1 (make scoped-test)"),
                    "{message}"
                );
            }
            other => panic!("expected queued, got {other:?}"),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn fifo_admission_with_position_and_slot_reason() {
        let (_w, g) = World::new();
        let p = GovernorPolicy {
            build_slots: 2,
            ..GovernorPolicy::LINUX_DEFAULT
        };
        let a = granted(&g.acquire(&p, &req(AdmissionClass::Build, 1)).unwrap());
        let _b = granted(&g.acquire(&p, &req(AdmissionClass::Build, 2)).unwrap());
        let (t3, pos3, why3) = queued(&g.acquire(&p, &req(AdmissionClass::Build, 3)).unwrap());
        let (t4, pos4, why4) = queued(&g.acquire(&p, &req(AdmissionClass::Build, 4)).unwrap());
        assert_eq!(pos3, 0);
        // #618: the reason names who holds the slots, longest-held first.
        let BlockReason::Slots {
            class,
            in_use,
            cap,
            holders,
        } = &why3
        else {
            panic!("expected Slots, got {why3:?}");
        };
        assert_eq!((*class, *in_use, *cap), (AdmissionClass::Build, 2, 2));
        let held_pids: std::collections::BTreeSet<u32> = holders.iter().map(|h| h.pid).collect();
        assert_eq!(held_pids, [1, 2].into());
        let text = why3.describe();
        assert!(
            text.contains("all 2 build slots busy (2 in use); held by "),
            "{text}"
        );
        assert!(
            text.contains(&format!("pid {}", 1)) && text.contains("pid 2"),
            "{text}"
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

    /// #618: a lease's unit is read from its cgroup path; a pid outside a job
    /// unit names none, and a busy-pool reason with no holders stays terse.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn slot_holder_unit_comes_from_the_cgroup_path() {
        let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/rsi-build.slice/rsi-job-0a1b2c3d-0000-4000-8000-000000000001.service\n";
        assert_eq!(
            unit_in_cgroup(cgroup).as_deref(),
            Some("rsi-job-0a1b2c3d-0000-4000-8000-000000000001.service")
        );
        assert_eq!(unit_in_cgroup("0::/user.slice/session-3.scope\n"), None);
        let bare = BlockReason::Slots {
            class: AdmissionClass::Build,
            in_use: 1,
            cap: 1,
            holders: Vec::new(),
        };
        assert_eq!(bare.describe(), "all 1 build slots busy (1 in use)");
        let held = BlockReason::Slots {
            class: AdmissionClass::Build,
            in_use: 1,
            cap: 1,
            holders: vec![SlotHolder {
                lease_id: Uuid::nil(),
                pid: 77,
                label: Some("cargo test".into()),
                held_secs: 90,
                unit: Some("rsi-job-x.service".into()),
            }],
        };
        let text = held.describe();
        assert!(
            text.contains("pid 77 (cargo test, unit rsi-job-x.service) for 90s"),
            "{text}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn classes_use_separate_pools() {
        let (_w, g) = World::new();
        let p = GovernorPolicy {
            build_slots: 1,
            lander_slots: 1,
            ..GovernorPolicy::LINUX_DEFAULT
        };
        granted(&g.acquire(&p, &req(AdmissionClass::Build, 1)).unwrap());
        // A full build pool does not block a lander.
        granted(&g.acquire(&p, &req(AdmissionClass::Lander, 2)).unwrap());
        queued(&g.acquire(&p, &req(AdmissionClass::Build, 3)).unwrap());
    }

    /// #2430: a worker's `cargo check` is granted while every build slot is
    /// held, and a closed load gate (what saturated builds produce) does not
    /// hold it; the disk and memory floors still do.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn check_lane_is_granted_while_build_slots_and_load_gate_are_closed() {
        let (w, g) = World::new();
        let p = GovernorPolicy {
            build_slots: 1,
            check_slots: 1,
            ..GovernorPolicy::LINUX_DEFAULT
        };
        granted(&g.acquire(&p, &req(AdmissionClass::Build, 1)).unwrap());
        queued(&g.acquire(&p, &req(AdmissionClass::Build, 2)).unwrap());
        w.set(|s| s.load1 = 1000.0);
        let check = granted(&g.acquire(&p, &req(AdmissionClass::Check, 3)).unwrap());
        // The lane has its own cap and FIFO: a second check waits for the first.
        let (waiting, pos, why) = queued(&g.acquire(&p, &req(AdmissionClass::Check, 4)).unwrap());
        assert_eq!(pos, 0);
        assert!(matches!(
            why,
            BlockReason::Slots {
                class: AdmissionClass::Check,
                in_use: 1,
                cap: 1,
                ..
            }
        ));
        assert!(g.release(&p, check));
        granted(&poll(&g, &p, waiting, AdmissionClass::Check));
        let snap = g.snapshot(&p);
        assert_eq!((snap.check_in_use, snap.build_in_use), (1, 1));
        // The memory floor applies to the check lane too.
        w.set(|s| s.mem_available_gb = 1.0);
        let (_t, _, why) = queued(&g.acquire(&p, &req(AdmissionClass::Check, 5)).unwrap());
        assert_eq!(
            serde_json::to_value(&why).unwrap()["gate"],
            "memory_available"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn load_gate_still_blocks_build_and_lander_classes() {
        let p = GovernorPolicy::LINUX_DEFAULT;
        let mut s = healthy();
        s.load1 = 1000.0;
        for class in [AdmissionClass::Build, AdmissionClass::Lander] {
            assert!(matches!(
                global_block(&p, &s, class),
                Some(BlockReason::Load { .. })
            ));
        }
        assert!(global_block(&p, &s, AdmissionClass::Check).is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn disk_floor_stall_latches_one_warning_until_the_gate_reopens() {
        let (w, g) = World::new();
        let p = GovernorPolicy::LINUX_DEFAULT;
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
    fn disk_wait_reports_exact_bytes_when_free_gib_rounds_to_the_floor() {
        let (w, mut g) = World::new();
        g.growth_probe = Box::new(|_| disk_growth::Usage {
            expected_bytes: 24 * 1_073_741_824,
            current_bytes: 24 * 1_073_741_824,
        });
        let p = GovernorPolicy::LINUX_DEFAULT;
        w.set(|s| s.disk_free_gb = (30.0 * GIB - 1.0) / GIB);
        let outcome = g.acquire(&p, &req(AdmissionClass::Build, 1)).unwrap();
        let (ticket, _, reason) = queued(&outcome);
        assert_eq!(
            reason.describe(),
            "only 30.0 GiB (32212254719 bytes) free on /, floor 30 GiB (32212254720 bytes); waiting for space"
        );
        w.set(|s| s.disk_free_gb = 30.0);
        granted(&poll(&g, &p, ticket, AdmissionClass::Build));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn each_host_gate_blocks_with_its_reason() {
        let p = GovernorPolicy::LINUX_DEFAULT;
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
    fn disk_wait_tracks_the_continuous_hold_and_resets_on_other_gates() {
        let (world, governor) = World::new();
        let policy = GovernorPolicy::LINUX_DEFAULT;
        world.set(|s| s.load1 = 40.0);
        let (ticket, _, _) = queued(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 1))
                .unwrap(),
        );
        assert_eq!(
            governor.snapshot(&policy).queue[0].disk_wait_started_at,
            None
        );
        world.set(|s| {
            *s = healthy();
            s.disk_free_gb = 27.0;
        });
        let snapshot = governor.snapshot(&policy);
        let since = snapshot.queue[0].disk_wait_started_at.unwrap();
        assert_eq!(
            governor.snapshot(&policy).queue[0].disk_wait_started_at,
            Some(since)
        );
        // Queue age is unrelated to the disk hold's age.
        let earlier = Utc::now() - chrono::Duration::minutes(40);
        governor.state().queue[0].disk_wait_started_at = Some(earlier);
        let snapshot = governor.snapshot(&policy);
        let build = crate::rolling_queue::satellite_qa::BuildGate::from_snapshot(&snapshot);
        assert!(build.disk_wait_seconds.unwrap() >= 2400);
        assert_eq!(build.disk_floor_gib, policy.min_free_disk_gb);
        assert_eq!(
            build.disk_free_bytes,
            Some(27 * rsi_common::satellite_qa::GIB_BYTES)
        );
        world.set(|s| {
            *s = healthy();
            s.load1 = 40.0;
        });
        assert_eq!(
            governor.snapshot(&policy).queue[0].disk_wait_started_at,
            None
        );
        world.set(|s| *s = healthy());
        granted(&poll(&governor, &policy, ticket, AdmissionClass::Build));
    }

    /// #625: a waiter held only by the disk floor is reported with its hold's
    /// age, but not while a lease holds a slot (that holder may free space).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn disk_stalled_waiters_need_the_floor_alone_and_no_slot_holder() {
        let (world, governor) = World::new();
        let policy = GovernorPolicy::LINUX_DEFAULT;
        // Healthy host: pid 1 takes a slot.
        let lease = granted(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 1))
                .unwrap(),
        );
        world.set(|s| s.disk_free_gb = 26.9);
        queued(
            &governor
                .acquire(&policy, &req(AdmissionClass::Build, 2))
                .unwrap(),
        );
        governor.snapshot(&policy);
        let earlier = Utc::now() - chrono::Duration::minutes(40);
        governor.state().queue[0].disk_wait_started_at = Some(earlier);
        assert!(governor.disk_stalled_waiters().is_empty());
        // The holder leaves: only the floor holds the waiter, for ~40 minutes.
        assert!(governor.release(&policy, lease));
        let stalled = governor.disk_stalled_waiters();
        assert_eq!(stalled.len(), 1, "{stalled:?}");
        assert!(stalled[0].stalled_secs >= 2400);
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn gate_thresholds_are_the_cargo_slot_boundaries() {
        let p = GovernorPolicy::LINUX_DEFAULT;
        assert!(global_block(&p, &healthy(), AdmissionClass::Build).is_none());
        let mut s = healthy();
        s.disk_free_gb = 30.0;
        s.load1 = 39.9;
        s.mem_available_gb = 16.0;
        s.workers_slice_anon_gb = Some(29.9);
        assert!(global_block(&p, &s, AdmissionClass::Build).is_none());
        s.load1 = 40.0;
        assert!(matches!(
            global_block(&p, &s, AdmissionClass::Build),
            Some(BlockReason::Load { .. })
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn unreadable_workers_slice_does_not_block() {
        let (w, g) = World::new();
        w.set(|s| s.workers_slice_anon_gb = None);
        granted(
            &g.acquire(
                &GovernorPolicy::LINUX_DEFAULT,
                &req(AdmissionClass::Build, 1),
            )
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
            ..GovernorPolicy::LINUX_DEFAULT
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
            ..GovernorPolicy::LINUX_DEFAULT
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
                swap_free_gb: 0.0,
                workers_slice_anon_gb: None,
            }),
            Box::new(move |pid| {
                Some(Holder {
                    pid,
                    start_ticks: *s.lock().unwrap(),
                })
            }),
        );
        let p = GovernorPolicy::LINUX_DEFAULT;
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
            ..GovernorPolicy::LINUX_DEFAULT
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
        let p = GovernorPolicy::LINUX_DEFAULT;
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
            ..GovernorPolicy::LINUX_DEFAULT
        };
        granted(&g.acquire(&one, &req(AdmissionClass::Build, 1)).unwrap());
        let (t, _, _) = queued(&g.acquire(&one, &req(AdmissionClass::Build, 2)).unwrap());
        granted(&poll(
            &g,
            &GovernorPolicy::LINUX_DEFAULT,
            t,
            AdmissionClass::Build,
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn snapshot_reports_margins_and_queue() {
        let (w, g) = World::new();
        let p = GovernorPolicy::LINUX_DEFAULT;
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
        std::path::Path::new(rsi_common::manifest_dir!()).join("../../scripts/cargo-slot")
    }

    /// The `rsi-repo-guard` the script's helper subcommands run (#465: the governor client,
    /// probes and ticket head moved out of inline interpreter code). Built once through the
    /// repo shim with the test's own cargo setup; `None` when it cannot be built.
    fn guard_bin() -> Option<std::path::PathBuf> {
        static BIN: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
        BIN.get_or_init(|| {
            let shim =
                std::path::Path::new(rsi_common::manifest_dir!()).join("../../scripts/repo-guard");
            let out = Command::new(shim)
                .arg("--bin")
                .stderr(Stdio::null())
                .output()
                .ok()?;
            let path = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            (out.status.success() && path.is_file()).then_some(path)
        })
        .clone()
    }

    fn tools_available() -> bool {
        ["ionice", "flock"].iter().all(|tool| {
            Command::new("sh")
                .args(["-c", &format!("command -v {tool}")])
                .stdout(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        }) && guard_bin().is_some()
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
            .env(
                "RSI_REPO_GUARD_BIN",
                guard_bin().expect("tools_available checked the guard"),
            )
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
            ..GovernorPolicy::LINUX_DEFAULT
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
