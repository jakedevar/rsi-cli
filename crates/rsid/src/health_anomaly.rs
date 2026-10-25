//! `when.health_anomaly` (#2747): the daemon-side evaluator that replaces a
//! manager's hourly safety-net model turn.
//!
//! Three layers keep every condition testable by injection:
//!
//! * [`StoreFacts`] are read from the database ([`collect_store_facts`]);
//! * [`HostReading`] and [`LiveHealthInputs`] are what the store cannot see
//!   (disk and CPU of this host, a PM's live context fill against its rotation
//!   threshold, each satellite's reported disk and provider faults); the
//!   session launcher supplies the live part;
//! * [`evaluate`] is a pure function of those inputs, the operator thresholds,
//!   the owner's `ignore` list and the "held for N seconds" tracker.
//!
//! No model is involved at any step; the scheduler resumes the owner once,
//! only when [`evaluate`] returns a condition.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};
use rsi_common::health_anomaly::{
    CONTEXT_FULL_GRACE_SECS, CRITICAL_DISK_FREE_GIB, CRITICAL_DISK_USED_PCT, CRITICAL_THERMAL_C,
    HEALTH_MAX_CONDITIONS, HUB_PULL_STALE_SECS, HealthAnomalySpec, HealthCondition,
    KIND_ATTEMPT_PAST_WALL_LIMIT, KIND_CPU_CRITICAL, KIND_CPU_HOT, KIND_DAEMON_RESTART_LOOP,
    KIND_EXECUTION_BLOCKED, KIND_HELPER_LLM_UNREACHABLE, KIND_HUB_DISK_CRITICAL,
    KIND_HUB_DISK_HIGH, KIND_HUB_MEMORY_HIGH, KIND_HUB_NOT_PULLING, KIND_MANAGER_CREATION_CAP_NEAR,
    KIND_MANAGER_DECISION_PENDING, KIND_PM_CONTEXT_FULL, KIND_PM_SEAT_BLOCKED,
    KIND_PROVIDER_AUTH_FAILURE, KIND_PROVIDER_MODEL_UNAVAILABLE, KIND_RECLAIM_UNSUPPORTED,
    KIND_ROLLING_QA_RED, KIND_SATELLITE_DISK_LOW, KIND_SATELLITE_MANAGER_NO_LAUNCHES,
    KIND_SUPERVISOR_RESTART_BUDGET, RESTART_LOOP_MAX_LISTED, RESTART_LOOP_STOPS,
    RESTART_LOOP_WINDOW_SECS, THERMAL_SUSTAIN_SECS,
};
use rsi_common::manager_budget::{CREATION_CAP_WARN_PCT, CreationCapUsageV1};
use rsi_common::rpc::ProviderRateLimitSnapshot;
use rsi_common::satellite::{
    SATELLITE_FAULT_AUTH, SATELLITE_FAULT_MODEL_UNAVAILABLE, SATELLITE_MAX_PROVIDER_FAULTS,
    SatelliteHealthV1, SatelliteProviderFaultV1,
};
use rsi_common::types::SessionStatus;
use rsid_core::health_thresholds::HealthThresholds;
use rusqlite::params;
use uuid::Uuid;

use crate::daemon_lifecycle::{DaemonStop, unannounced_within};
use crate::error::Result;
use crate::store::Store;
use crate::store::satellite_reports::HubReportLinkStatus;
use rsi_common::satellite_dispatch::SatelliteReportStatusV1;

/// What a wake may observe (#2788): the projects the owner's grant covers
/// right now. Project-bound facts (seats, blocked executions, overrun
/// attempts, a red QA, a PM's context) are limited to `projects`; the host's
/// own readings and satellite disk/provider readings are fleet-wide. Satellite
/// manager failures also reach the matching project manager. An area additionally
/// limits work to its direct Epics and seat facts to its own seat.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scope {
    projects: Vec<Uuid>,
    fleet_wide: bool,
    area_seat: Option<Uuid>,
    epic_ids: Option<Vec<Uuid>>,
}

impl Scope {
    /// A portfolio seat covering exactly `projects`.
    #[must_use]
    pub fn portfolio(mut projects: Vec<Uuid>) -> Self {
        projects.sort();
        projects.dedup();
        Self {
            projects,
            fleet_wide: true,
            ..Self::default()
        }
    }

    /// One project manager's own project.
    #[must_use]
    pub fn project(project: Uuid) -> Self {
        Self {
            projects: vec![project],
            fleet_wide: false,
            ..Self::default()
        }
    }

    /// Observes nothing project-bound: the owner no longer covers anything.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn allows(&self, project: Uuid) -> bool {
        self.projects.contains(&project)
    }

    #[must_use]
    pub fn projects(&self) -> &[Uuid] {
        &self.projects
    }

    #[must_use]
    pub fn is_fleet_wide(&self) -> bool {
        self.fleet_wide
    }

    /// An area watches its own seat and work under its live direct Epics.
    #[must_use]
    pub fn area_seat(&self) -> Option<Uuid> {
        self.area_seat
    }

    /// The scope `owner` may observe now for the stored `spec`: the stored
    /// projects intersected with the owner's current portfolio grant, so a
    /// narrowed or revoked grant narrows the wake and a seat that never held
    /// the project never sees it. A project manager keeps its stored project
    /// only while it still holds the project seat. An area re-resolves its
    /// current direct Epics and seat on every evaluation. A spec armed before
    /// `projects` existed (portfolio, nothing stored) follows the live grant.
    ///
    /// # Errors
    /// A persistence error.
    pub fn live(store: &Store, owner: Option<Uuid>, spec: &HealthAnomalySpec) -> Result<Self> {
        let Some(owner) = owner else {
            return Ok(Self::none());
        };
        let area = match store.manager_area_authority_current(owner) {
            Ok(area) => area,
            Err(crate::error::DaemonError::InvalidParam(_)) => None,
            Err(error) => return Err(error),
        };
        if let Some(area) = area {
            return Ok(if spec.project == Some(area.config.project_id) {
                Self {
                    projects: vec![area.config.project_id],
                    fleet_wide: false,
                    area_seat: Some(owner),
                    epic_ids: Some(area.config.epic_ids),
                }
            } else {
                Self::none()
            });
        }
        let grant = store.portfolio_seat_grant(owner)?;
        if let Some(only) = spec.project {
            let covered = match &grant {
                Some(grant) => grant.project_ids.contains(&only),
                None => store.get_harness_manager(only)?.is_some_and(|config| {
                    config.current_session_id == Some(owner) && !config.is_revoked()
                }),
            };
            return Ok(if covered {
                Self::project(only)
            } else {
                Self::none()
            });
        }
        let Some(grant) = grant else {
            return Ok(Self::none());
        };
        let covered = grant
            .project_ids
            .into_iter()
            .filter(|project| spec.projects.is_empty() || spec.projects.contains(project))
            .collect();
        Ok(Self::portfolio(covered))
    }
}

/// A PM seat whose newest root succession is finally refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatFact {
    pub project_id: Uuid,
    /// `None` = no seat session exists.
    pub seat_status: Option<SessionStatus>,
    pub refusal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedExecution {
    pub execution_id: Uuid,
    pub blocked_since: DateTime<Utc>,
    pub reason_kind: Option<String>,
}

/// A decision a manager may rule on, still pending (#3240).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingManagerDecision {
    pub key: String,
    pub project_id: Uuid,
    pub asked_at: DateTime<Utc>,
    /// The asking topology execution, if any.
    pub execution_id: Option<Uuid>,
}

/// #206: one of a project manager seat's own creation caps, as
/// [`Store::manager_own_creation_caps`] counts it (the refusal's counter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreationCapFact {
    pub project_id: Uuid,
    pub cap: CreationCapUsageV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrunAttempt {
    pub attempt_id: Uuid,
    pub execution_id: Uuid,
    pub node_id: String,
    pub deadline: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedQa {
    pub sha: String,
    pub finished_at: DateTime<Utc>,
}

/// A provider fault on one host (`hub` or a satellite peer id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFault {
    pub host: String,
    pub provider: String,
    /// [`SATELLITE_FAULT_AUTH`] or [`SATELLITE_FAULT_MODEL_UNAVAILABLE`].
    pub kind: &'static str,
    pub model: Option<String>,
}

/// Facts the database answers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreFacts {
    pub satellite_failed_seats: Vec<HealthCondition>,
    pub hub_link: Option<HubReportLinkStatus>,
    pub provider_windows: Vec<ProviderRateLimitSnapshot>,
    pub seats: Vec<SeatFact>,
    pub blocked_executions: Vec<BlockedExecution>,
    pub overrun_attempts: Vec<OverrunAttempt>,
    pub pending_manager_decisions: Vec<PendingManagerDecision>,
    pub creation_caps: Vec<CreationCapFact>,
    pub hub_provider_faults: Vec<ProviderFault>,
    pub red_qa: Option<RedQa>,
}

/// This host's disk and CPU now; `None` = unreadable (nothing fires).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostReading {
    pub disk_used_pct: Option<f64>,
    pub cpu_c: Option<f64>,
    /// `MemAvailable` in GiB; `None` = unreadable.
    pub mem_available_gb: Option<f64>,
    /// Swap use in percent; `None` = no swap or unreadable.
    pub swap_used_pct: Option<f64>,
    /// The governor's live `min_avail_mem_gb` floor; 0 turns the available
    /// arm of `hub_memory_high` off.
    pub mem_floor_gb: u64,
    /// Free GiB on `/`; `None` = unreadable (no critical-free-space condition).
    pub disk_free_gb: Option<f64>,
    /// This build cannot reclaim sandbox build caches (the fail-closed
    /// `target_reclaim_unsupported.rs` shim; hub #3055 part 2). False on
    /// Linux and macOS.
    pub build_cache_reclaim_unsupported: bool,
}

/// A PM seat's live context against its rotation threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct PmContext {
    pub project_id: Uuid,
    pub session_id: Uuid,
    pub fill_pct: f64,
    pub threshold_pct: f64,
    /// A root succession for this seat is already unresolved (queued).
    pub succession_queued: bool,
}

/// One satellite as last read over its link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SatelliteReading {
    pub peer_id: Uuid,
    pub label: String,
    pub disk_free_bytes: Option<u64>,
    pub disk_floor_bytes: Option<u64>,
    pub disk_wait_seconds: Option<u64>,
    pub provider_faults: Vec<SatelliteProviderFaultV1>,
    pub manager_policies: Vec<rsi_common::satellite_manager_policy::SatelliteManagerLaunchesV1>,
}

impl SatelliteReading {
    /// A reading from the health a satellite reported.
    #[must_use]
    pub fn from_health(peer_id: Uuid, label: &str, health: &SatelliteHealthV1) -> Self {
        let admission = health.qa_admission.as_ref();
        let disk = health.governor_disk.as_ref();
        Self {
            peer_id,
            label: label.to_owned(),
            disk_free_bytes: disk.map_or_else(
                || {
                    admission
                        .and_then(|a| a.build_disk_free_bytes)
                        .or(health.disk_free_bytes)
                },
                |d| d.free_bytes,
            ),
            disk_floor_bytes: disk
                .map(|d| d.floor_bytes)
                .or_else(|| admission.map(|a| a.build_disk_floor_bytes)),
            disk_wait_seconds: disk.and_then(|d| d.oldest_wait_seconds),
            provider_faults: health.provider_faults.clone(),
            manager_policies: health.manager_policies.clone().unwrap_or_default(),
        }
    }
}

/// What only the live session manager can read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiveHealthInputs {
    pub pm_contexts: Vec<PmContext>,
    pub satellites: Vec<SatelliteReading>,
}

/// A local helper-LLM endpoint whose circuit breaker is open (#261), as
/// `memory::helper_breaker` holds it in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperOutage {
    /// `localhost:<port>`.
    pub endpoint: String,
    /// The purpose whose call opened the breaker, and the setting that chose
    /// its model.
    pub purpose: String,
    pub setting_key: String,
    pub model: String,
    /// Other purposes refused since, as `<purpose>(<setting>)`.
    pub other_purposes: Vec<String>,
    pub since: DateTime<Utc>,
    pub next_probe_in_secs: u64,
    pub failed_probes: u32,
}

/// Everything [`evaluate`] looks at.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub store: StoreFacts,
    pub host: HostReading,
    pub live: LiveHealthInputs,
    /// This daemon's open helper-LLM breakers.
    pub helper_outages: Vec<HelperOutage>,
    /// Every daemon stop the lifecycle journal holds (#3480).
    pub daemon_stops: Vec<DaemonStop>,
    /// The restart history the supervisor published (#3492); `None` when it
    /// publishes none (no supervisor, or one that predates the file).
    pub supervisor_restarts: Option<rsi_common::supervisor_restarts::SupervisorRestarts>,
}

/// "Held for N seconds" bookkeeping: the first time each condition key was
/// seen active. An inactive key is forgotten, so a flap restarts the clock.
#[derive(Debug, Default)]
pub struct Sustain {
    first_seen: HashMap<String, DateTime<Utc>>,
    pub(crate) windows: provider_windows::WindowTracker,
}

impl Sustain {
    /// True once `key` has been `active` continuously for `secs`.
    pub fn held(&mut self, key: &str, active: bool, now: DateTime<Utc>, secs: i64) -> bool {
        if !active {
            self.first_seen.remove(key);
            return false;
        }
        let first = *self.first_seen.entry(key.to_owned()).or_insert(now);
        now - first >= Duration::seconds(secs)
    }
}

/// The scheduler's trackers, one per armed wake (job id): each wake's
/// "held for N seconds" clocks are its own, and a wake that fires drops its
/// clocks, so a condition that persists after a re-arm must hold again.
pub fn sustain() -> &'static Mutex<HashMap<Uuid, Sustain>> {
    static SUSTAIN: OnceLock<Mutex<HashMap<Uuid, Sustain>>> = OnceLock::new();
    SUSTAIN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Outage acknowledgements outlive individual wakes. Like the report outbox's
/// last-pull clock, these are process-local: restarting starts a new observation.
/// The successful-pull timestamp is the outage generation, so recovery need not
/// coincide with a scheduler evaluation. No capability or hub grant is stored.
#[derive(Default)]
pub(crate) struct HubPullOutages {
    notified: HashMap<(Uuid, Uuid), (Option<String>, Uuid)>,
}

impl HubPullOutages {
    pub(crate) fn evaluate(
        &mut self,
        observer: (Uuid, Uuid), // manager, wake (retries of this wake still deliver)
        inputs: &Inputs,
        thresholds: &HealthThresholds,
        spec: &HealthAnomalySpec,
        sustain: &mut Sustain,
        now: DateTime<Utc>,
    ) -> Vec<HealthCondition> {
        let (owner, wake) = observer;
        let link = inputs.store.hub_link.as_ref();
        self.notified.retain(|(manager, hub), (pulled, _)| {
            *manager != owner
                || link.is_some_and(|link| {
                    link.hub_installations.contains(hub)
                        && hub_pull_stale(&link.reports, now)
                        && *pulled == link.reports.last_hub_pull_at
                })
        });
        let mut effective = spec.clone();
        for ((manager, hub), (_, notified_wake)) in &self.notified {
            if *manager == owner && *notified_wake != wake {
                effective
                    .ignore
                    .push(format!("{KIND_HUB_NOT_PULLING}:{hub}"));
            }
        }
        // Suppress before the condition limit so latched hubs cannot crowd out
        // other health conditions. Explicit ignore retains its existing meaning.
        let found = evaluate(inputs, thresholds, &effective, sustain, now);
        if let Some(link) = link {
            for condition in &found {
                if condition.kind == KIND_HUB_NOT_PULLING
                    && let Ok(hub) = Uuid::parse_str(&condition.subject)
                {
                    self.notified
                        .insert((owner, hub), (link.reports.last_hub_pull_at.clone(), wake));
                }
            }
        }
        found
    }
}

/// #4020: the anomaly set the daemon-armed manager idle backstop (#3765)
/// already woke each seat for. A seat that finishes under a persistent anomaly
/// has its backstop re-armed by the next turn end (a new wake row); without
/// this latch the fresh row would be satisfied at once by the same unchanged
/// set and resume the seat again, about every pass. Like [`HubPullOutages`]
/// the latch is process-local (a restart starts a new observation) and keyed
/// by the seat, so it outlives the individual wake rows.
#[derive(Default)]
pub(crate) struct IdleBackstopLatch {
    /// seat -> (condition keys it was woken for, the wake that woke it).
    fired: HashMap<Uuid, (BTreeSet<String>, Uuid)>,
}

impl IdleBackstopLatch {
    /// The conditions that are news to `seat` for this evaluation of `wake`.
    /// A key a *different* wake already reported stays quiet while the
    /// condition holds; it is forgotten once the condition clears, so its
    /// return fires again. A retry of the reporting wake still delivers.
    pub(crate) fn filter(
        &mut self,
        seat: Uuid,
        wake: Uuid,
        found: Vec<HealthCondition>,
    ) -> Vec<HealthCondition> {
        let current: BTreeSet<String> = found.iter().map(HealthCondition::key).collect();
        let mut latched = BTreeSet::new();
        if let Some((keys, reporter)) = self.fired.get_mut(&seat)
            && *reporter != wake
        {
            keys.retain(|key| current.contains(key));
            latched.clone_from(keys);
        }
        if latched.is_empty()
            && self
                .fired
                .get(&seat)
                .is_some_and(|(_, reporter)| *reporter != wake)
        {
            self.fired.remove(&seat);
        }
        let fresh: Vec<HealthCondition> = found
            .into_iter()
            .filter(|condition| !latched.contains(&condition.key()))
            .collect();
        if !fresh.is_empty() {
            self.fired.insert(seat, (current, wake));
        }
        fresh
    }
}

pub(crate) fn idle_backstop_latch() -> &'static Mutex<IdleBackstopLatch> {
    static LATCH: OnceLock<Mutex<IdleBackstopLatch>> = OnceLock::new();
    LATCH.get_or_init(|| Mutex::new(IdleBackstopLatch::default()))
}

pub(crate) fn hub_pull_outages() -> &'static Mutex<HubPullOutages> {
    static OUTAGES: OnceLock<Mutex<HubPullOutages>> = OnceLock::new();
    OUTAGES.get_or_init(|| Mutex::new(HubPullOutages::default()))
}

fn pct(value: f64) -> String {
    format!("{value:.1}%")
}

/// `hub_memory_high`: low `MemAvailable` against the governor floor, or swap
/// nearly full (hub #3629). Pure over [`HostReading`]; nothing fires on an
/// unreadable source.
fn memory_high_condition(host: &HostReading) -> Option<HealthCondition> {
    use rsi_common::health_anomaly::{MEMORY_HIGH_AVAILABLE_FRACTION, MEMORY_HIGH_SWAP_USED_PCT};
    let low_available = host.mem_available_gb.filter(|available| {
        host.mem_floor_gb > 0
            && *available < host.mem_floor_gb as f64 * MEMORY_HIGH_AVAILABLE_FRACTION
    });
    let swap_high = host
        .swap_used_pct
        .filter(|used| *used >= MEMORY_HIGH_SWAP_USED_PCT);
    if low_available.is_none() && swap_high.is_none() {
        return None;
    }
    let mut parts = Vec::new();
    if let Some(available) = low_available {
        parts.push(format!(
            "mem_available={available:.1}GiB below {:.1}GiB (half of governor floor {}GiB)",
            host.mem_floor_gb as f64 * MEMORY_HIGH_AVAILABLE_FRACTION,
            host.mem_floor_gb
        ));
    }
    if let Some(used) = swap_high {
        parts.push(format!(
            "swap_used={} threshold={MEMORY_HIGH_SWAP_USED_PCT:.0}%",
            pct(used)
        ));
    }
    Some(HealthCondition::new(
        KIND_HUB_MEMORY_HIGH,
        "hub",
        parts.join(" "),
    ))
}

/// A known pull past the link deadline, even with no pending reports. Before
/// the first pull, an aged pending report is evidence that the hub is missing.
/// A newly paired idle satellite has no evidence of a failed link yet.
pub(crate) fn hub_pull_stale(status: &SatelliteReportStatusV1, now: DateTime<Utc>) -> bool {
    status
        .last_hub_pull_at
        .as_deref()
        .or(status.backlog.oldest_queued_at.as_deref())
        .and_then(parse_time)
        .is_some_and(|at| now - at > Duration::seconds(HUB_PULL_STALE_SECS))
}

/// Every condition that holds now, minus what `spec.ignore` accepts, most
/// urgent first and bounded by [`HEALTH_MAX_CONDITIONS`].
#[must_use]
pub fn evaluate(
    inputs: &Inputs,
    thresholds: &HealthThresholds,
    spec: &HealthAnomalySpec,
    sustain: &mut Sustain,
    now: DateTime<Utc>,
) -> Vec<HealthCondition> {
    let mut found: Vec<HealthCondition> = Vec::new();
    let stuck = Duration::minutes(i64::from(thresholds.stuck_minutes));

    found.extend(sustain.windows.evaluate(
        &inputs.store.provider_windows,
        thresholds.provider_window_used_pct,
        now,
    ));

    if let Some(link) = &inputs.store.hub_link
        && hub_pull_stale(&link.reports, now)
    {
        for hub in &link.hub_installations {
            found.push(HealthCondition::new(
                KIND_HUB_NOT_PULLING,
                hub.to_string(),
                format!(
                    "last_hub_pull_at={} backlog_depth={} oldest_age_secs={:?} threshold_secs={HUB_PULL_STALE_SECS}",
                    link.reports.last_hub_pull_at.as_deref().unwrap_or("never"),
                    link.reports.backlog.depth,
                    link.reports.backlog.oldest_age_secs,
                ),
            ));
        }
    }

    for seat in &inputs.store.seats {
        let status = seat
            .seat_status
            .map_or_else(|| "vacant".to_owned(), |s| format!("{s:?}"));
        found.push(HealthCondition::new(
            KIND_PM_SEAT_BLOCKED,
            seat.project_id.to_string(),
            format!("seat={status} succession_refusal={}", seat.refusal),
        ));
    }

    // #206: warn before the creation wall, not at it.
    for fact in &inputs.store.creation_caps {
        if fact.cap.near_limit() {
            found.push(HealthCondition::new(
                KIND_MANAGER_CREATION_CAP_NEAR,
                format!("{}:{}", fact.project_id, fact.cap.cap_field),
                format!(
                    "cap={} used={} limit={} warn_at={CREATION_CAP_WARN_PCT}% raise_in={}{}",
                    fact.cap.cap_field,
                    fact.cap.used,
                    fact.cap.limit,
                    fact.cap.raise_in,
                    // #3416: a windowed cap refills; say when.
                    fact.cap
                        .window_summary()
                        .map(|window| format!(" window=\"{window}\""))
                        .unwrap_or_default()
                ),
            ));
        }
    }

    for pm in &inputs.live.pm_contexts {
        let over = pm.fill_pct >= pm.threshold_pct && !pm.succession_queued;
        let key = format!("{KIND_PM_CONTEXT_FULL}:{}", pm.session_id);
        if sustain.held(&key, over, now, CONTEXT_FULL_GRACE_SECS) {
            found.push(HealthCondition::new(
                KIND_PM_CONTEXT_FULL,
                pm.session_id.to_string(),
                format!(
                    "project={} context_fill_pct={:.1} rotation_threshold_pct={:.1} succession=none",
                    pm.project_id, pm.fill_pct, pm.threshold_pct
                ),
            ));
        }
    }

    if thresholds.stuck_minutes > 0 {
        for execution in &inputs.store.blocked_executions {
            if now - execution.blocked_since >= stuck {
                found.push(HealthCondition::new(
                    KIND_EXECUTION_BLOCKED,
                    execution.execution_id.to_string(),
                    format!(
                        "blocked_since={} blocked_kind={}",
                        execution.blocked_since.to_rfc3339(),
                        execution.reason_kind.as_deref().unwrap_or("-")
                    ),
                ));
            }
        }
        for decision in &inputs.store.pending_manager_decisions {
            if now - decision.asked_at >= stuck {
                found.push(HealthCondition::new(
                    KIND_MANAGER_DECISION_PENDING,
                    decision.key.clone(),
                    format!(
                        "project={} asked_at={} execution={} rule_with=AgentManagerUpdate:decision_ruling",
                        decision.project_id,
                        decision.asked_at.to_rfc3339(),
                        decision
                            .execution_id
                            .map_or_else(|| "-".to_owned(), |id| id.to_string())
                    ),
                ));
            }
        }
        for attempt in &inputs.store.overrun_attempts {
            if now - attempt.deadline >= stuck {
                found.push(HealthCondition::new(
                    KIND_ATTEMPT_PAST_WALL_LIMIT,
                    attempt.attempt_id.to_string(),
                    format!(
                        "execution={} node={} deadline={}",
                        attempt.execution_id,
                        attempt.node_id,
                        attempt.deadline.to_rfc3339()
                    ),
                ));
            }
        }
        if let Some(qa) = &inputs.store.red_qa
            && now - qa.finished_at >= stuck
        {
            found.push(HealthCondition::new(
                KIND_ROLLING_QA_RED,
                qa.sha.clone(),
                format!("finished_at={} no_open_issue", qa.finished_at.to_rfc3339()),
            ));
        }
    }

    if thresholds.disk_used_pct > 0
        && let Some(used) = inputs.host.disk_used_pct
        && used >= f64::from(thresholds.disk_used_pct)
    {
        found.push(disk_condition(
            used,
            inputs.host.disk_free_gb,
            thresholds.disk_used_pct,
        ));
    } else if thresholds.disk_used_pct > 0
        && let Some(free) = inputs.host.disk_free_gb
        && free < CRITICAL_DISK_FREE_GIB
    {
        // Free space can be critical while the percent is still below the
        // warm threshold (a very small disk, a raised threshold).
        found.push(disk_condition(
            inputs.host.disk_used_pct.unwrap_or_default(),
            Some(free),
            thresholds.disk_used_pct,
        ));
    }

    found.extend(memory_high_condition(&inputs.host));

    let hot = thresholds.thermal_c > 0
        && inputs
            .host
            .cpu_c
            .is_some_and(|c| c >= f64::from(thresholds.thermal_c));
    if sustain.held(KIND_CPU_HOT, hot, now, THERMAL_SUSTAIN_SECS) {
        let cpu_c = inputs.host.cpu_c.unwrap_or_default();
        // Critical replaces the warm line, under its own ignore key, so an
        // ignored warm `cpu_hot:hub` never hides it (hub #3634).
        let kind = if cpu_c >= CRITICAL_THERMAL_C {
            KIND_CPU_CRITICAL
        } else {
            KIND_CPU_HOT
        };
        found.push(HealthCondition::new(
            kind,
            "hub",
            format!(
                "tctl={cpu_c:.1}C threshold={}C critical_at={CRITICAL_THERMAL_C:.0}C severity={} sustained={}s",
                thresholds.thermal_c,
                if kind == KIND_CPU_CRITICAL { "critical" } else { "warm" },
                THERMAL_SUSTAIN_SECS
            ),
        ));
    }

    if inputs.host.build_cache_reclaim_unsupported {
        found.push(HealthCondition::new(
            KIND_RECLAIM_UNSUPPORTED,
            "hub",
            format!(
                "platform={} build_cache_reclaim=unsupported: sandbox target/ caches are \
                 never reclaimed automatically; free space with scripts/clean-target",
                std::env::consts::OS
            ),
        ));
    }

    for outage in &inputs.helper_outages {
        let others = if outage.other_purposes.is_empty() {
            String::new()
        } else {
            format!(" also={}", outage.other_purposes.join(","))
        };
        found.push(HealthCondition::new(
            KIND_HELPER_LLM_UNREACHABLE,
            outage.endpoint.clone(),
            format!(
                "purpose={} setting={} model={}{others} since={} failed_probes={} \
                 next_probe_in={}s fix={}",
                outage.purpose,
                outage.setting_key,
                outage.model,
                outage.since.to_rfc3339(),
                outage.failed_probes,
                outage.next_probe_in_secs,
                crate::memory::helper_breaker::fix_text(&outage.setting_key, &outage.model),
            ),
        ));
    }

    // #3492: the supervisor's one restart budget is spent.
    if let Some(published) = &inputs.supervisor_restarts {
        let now_secs = now.timestamp();
        let recent = published.in_window(now_secs);
        if published.exhausted(now_secs) {
            let newest = recent.last().copied().unwrap_or(now_secs);
            let frees = recent
                .len()
                .checked_sub(published.limit)
                .and_then(|index| recent.get(index))
                .map(|oldest| oldest + published.window_secs)
                .into_iter()
                .chain(published.backoff_until)
                .max()
                .unwrap_or(now_secs);
            found.push(HealthCondition::new(
                KIND_SUPERVISOR_RESTART_BUDGET,
                format!("hub:{newest}"),
                format!(
                    "restarts_in_window={} limit={} window={}s backing_off={} frees_at_unix={frees} \
                     effect=deploys are refused (deploy_restart_budget) and the next crash \
                     restart waits the budget out instead of the supervisor exiting; \
                     look_at=daemon_restart_loop and the supervisor lines in the journal",
                    recent.len(),
                    published.limit,
                    published.window_secs,
                    published.backoff_until.is_some_and(|at| at > now_secs),
                ),
            ));
        }
    }

    // #3480: a daemon that keeps stopping without announcing it.
    let loop_stops = unannounced_within(
        &inputs.daemon_stops,
        now,
        Duration::seconds(RESTART_LOOP_WINDOW_SECS),
    );
    if loop_stops.len() >= RESTART_LOOP_STOPS
        && let Some(newest) = loop_stops.last()
    {
        let listed = loop_stops
            .iter()
            .rev()
            .take(RESTART_LOOP_MAX_LISTED)
            .map(|stop| {
                format!(
                    "[stopped_at={} uptime={} reason={} active={}]",
                    stop.stopped_at.to_rfc3339(),
                    stop.uptime_secs()
                        .map_or_else(|| "unknown".to_owned(), |secs| format!("{secs}s")),
                    stop.reason,
                    if stop.active.is_empty() {
                        "none_recorded".to_owned()
                    } else {
                        stop.active.join(",")
                    },
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        found.push(HealthCondition::new(
            KIND_DAEMON_RESTART_LOOP,
            format!("hub:{}", newest.stopped_at.to_rfc3339()),
            format!(
                "unannounced_stops={} window={}h newest_first={listed} \
                 look_at=the lander entries and job units active at each stop \
                 (a gate that signals the hub is re-gated on every start; \
                 the merge queue holds an entry suspect after two stops)",
                loop_stops.len(),
                RESTART_LOOP_WINDOW_SECS / 3600,
            ),
        ));
    }

    found.extend(inputs.store.satellite_failed_seats.iter().cloned());
    for satellite in &inputs.live.satellites {
        for manager in &satellite.manager_policies {
            if let Some(seat) = manager.manager_session_id
                && manager.allowed_launches == Some(0)
            {
                found.push(HealthCondition::new(
                    KIND_SATELLITE_MANAGER_NO_LAUNCHES,
                    format!("{}:{}", satellite.peer_id, manager.project_id),
                    format!(
                        "label={} manager_session_id={seat} scope_version={:?} policy_version={:?} allowed_launches=0 launches={}",
                        satellite.label, manager.scope_version, manager.policy_version, manager.launches,
                    ),
                ));
            }
        }
        let below_floor = matches!((satellite.disk_free_bytes, satellite.disk_floor_bytes),
            (Some(free), Some(floor)) if free < floor);
        let stuck_on_disk = thresholds.stuck_minutes > 0
            && satellite
                .disk_wait_seconds
                .is_some_and(|seconds| seconds > u64::from(thresholds.stuck_minutes) * 60);
        if below_floor || stuck_on_disk {
            found.push(HealthCondition::new(
                KIND_SATELLITE_DISK_LOW,
                satellite.peer_id.to_string(),
                format!(
                    "label={} free_bytes={:?} floor_bytes={:?} disk_wait_seconds={:?}",
                    satellite.label,
                    satellite.disk_free_bytes,
                    satellite.disk_floor_bytes,
                    satellite.disk_wait_seconds
                ),
            ));
        }
    }

    let satellite_faults = inputs.live.satellites.iter().flat_map(|satellite| {
        satellite.provider_faults.iter().filter_map(|fault| {
            let kind = match fault.kind.as_str() {
                SATELLITE_FAULT_AUTH => SATELLITE_FAULT_AUTH,
                SATELLITE_FAULT_MODEL_UNAVAILABLE => SATELLITE_FAULT_MODEL_UNAVAILABLE,
                _ => return None,
            };
            Some(ProviderFault {
                host: satellite.peer_id.to_string(),
                provider: fault.provider.clone(),
                kind,
                model: fault.model.clone(),
            })
        })
    });
    for fault in inputs
        .store
        .hub_provider_faults
        .iter()
        .cloned()
        .chain(satellite_faults)
    {
        found.push(if fault.kind == SATELLITE_FAULT_AUTH {
            HealthCondition::new(
                KIND_PROVIDER_AUTH_FAILURE,
                format!("{}:{}", fault.host, fault.provider),
                "auth rejected (401-class) on launch",
            )
        } else {
            HealthCondition::new(
                KIND_PROVIDER_MODEL_UNAVAILABLE,
                format!(
                    "{}:{}:{}",
                    fault.host,
                    fault.provider,
                    fault.model.as_deref().unwrap_or("-")
                ),
                "model refused as unavailable",
            )
        });
    }

    found.retain(|condition| {
        !spec.ignores(condition) && !provider_windows::ignored_episode(spec, condition)
    });
    found.truncate(HEALTH_MAX_CONDITIONS);
    found
}

/// The disk condition: critical (own kind, own ignore key, hub #3634) at or
/// above [`CRITICAL_DISK_USED_PCT`] used or below [`CRITICAL_DISK_FREE_GIB`]
/// free; otherwise the warm `hub_disk_high`. The detail names the severity.
fn disk_condition(used: f64, free_gb: Option<f64>, threshold_pct: u32) -> HealthCondition {
    let critical =
        used >= CRITICAL_DISK_USED_PCT || free_gb.is_some_and(|free| free < CRITICAL_DISK_FREE_GIB);
    let free = free_gb.map_or_else(String::new, |free| format!(" free={free:.1}GiB"));
    HealthCondition::new(
        if critical {
            KIND_HUB_DISK_CRITICAL
        } else {
            KIND_HUB_DISK_HIGH
        },
        "/",
        format!(
            "used={}{free} threshold={threshold_pct}% severity={} critical_at={CRITICAL_DISK_USED_PCT:.0}%_or_free<{CRITICAL_DISK_FREE_GIB:.0}GiB",
            pct(used),
            if critical { "critical" } else { "warm" }
        ),
    )
}

/// This host's disk use and CPU temperature now.
#[must_use]
pub fn read_host() -> HostReading {
    #[cfg(test)]
    if let Some(forced) = TEST_HOST.with(std::cell::Cell::get) {
        return forced;
    }
    HostReading {
        disk_used_pct: crate::host_pressure::disk_used_pct(),
        mem_available_gb: Some(crate::governor::read_mem_available_gb())
            .filter(|gb| gb.is_finite() && *gb < f64::MAX),
        swap_used_pct: crate::governor::read_swap_used_pct(),
        mem_floor_gb: crate::governor::GovernorPolicy::live_job_gate().min_avail_mem_gb,
        disk_free_gb: Some(crate::host_pressure::disk_free_gb())
            .filter(|free| free.is_finite() && *free < f64::MAX),
        cpu_c: match rsid_core::thermal_guard::current_reading() {
            rsid_core::thermal_guard::Reading::Celsius(c) => Some(c),
            rsid_core::thermal_guard::Reading::Unsupported => None,
        },
        build_cache_reclaim_unsupported: !crate::sandbox::target_reclaim::RECLAIM_SUPPORTED,
    }
}

// Test seam: the reading this thread sees, `None` = the real host.
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_HOST: std::cell::Cell<Option<HostReading>> =
        const { std::cell::Cell::new(None) };
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Auth episodes and refused models this host's launches hit, bounded.
///
/// # Errors
/// A persistence error.
pub fn local_provider_faults(
    store: &Store,
    host: &str,
    now: DateTime<Utc>,
) -> Result<Vec<ProviderFault>> {
    let mut faults = Vec::new();
    let mut auth: Vec<String> = store
        .provider_launch_stats(now)?
        .into_iter()
        .filter(|(_, stats)| {
            stats
                .auth_episode(now)
                .is_some_and(|episode| episode.refuses())
        })
        .map(|(provider, _)| provider)
        .collect();
    auth.sort();
    for provider in auth {
        faults.push(ProviderFault {
            host: host.to_owned(),
            provider,
            kind: SATELLITE_FAULT_AUTH,
            model: None,
        });
    }
    for refused in store.models_unavailable(now)? {
        faults.push(ProviderFault {
            host: host.to_owned(),
            provider: refused.provider,
            kind: SATELLITE_FAULT_MODEL_UNAVAILABLE,
            model: Some(refused.model),
        });
    }
    faults.truncate(SATELLITE_MAX_PROVIDER_FAULTS);
    Ok(faults)
}

/// How long the faults a satellite reports are reused: its identity is read
/// far more often than a provider fault changes.
const REPORTED_FAULTS_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// The faults a satellite reports on its identity health.
///
/// # Errors
/// A persistence error.
pub fn health_provider_faults(
    store: &Store,
    now: DateTime<Utc>,
) -> Result<Vec<SatelliteProviderFaultV1>> {
    type Cached = Option<(std::time::Instant, Vec<SatelliteProviderFaultV1>)>;
    static CACHE: OnceLock<Mutex<Cached>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Some((at, faults)) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        && at.elapsed() < REPORTED_FAULTS_TTL
    {
        return Ok(faults.clone());
    }
    let faults: Vec<SatelliteProviderFaultV1> = local_provider_faults(store, "satellite", now)?
        .into_iter()
        .map(|fault| SatelliteProviderFaultV1 {
            provider: fault.provider,
            kind: fault.kind.to_owned(),
            model: fault.model,
        })
        .collect();
    *cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((std::time::Instant::now(), faults.clone()));
    Ok(faults)
}

/// Read the database part of the inputs for `scope`.
///
/// # Errors
/// A persistence error.
pub fn collect_store_facts(store: &Store, scope: &Scope, now: DateTime<Utc>) -> Result<StoreFacts> {
    let mut facts = StoreFacts::default();
    if scope.is_fleet_wide() || (!scope.projects().is_empty() && scope.area_seat().is_none()) {
        facts.satellite_failed_seats = satellite_seats::collect(store, scope)?;
    }
    // Inbound hub health is local host health: project managers must hear it
    // too, rather than only portfolio seats that watch remote satellites.
    if !scope.projects().is_empty() {
        facts.hub_link = store.hub_report_link_status(now)?;
        facts.provider_windows = store.load_provider_rate_limit_snapshots()?;
        crate::codex_rate_limits::tracker().apply(&mut facts.provider_windows);
    }
    // Project-bound facts are limited to the scope by the query itself.
    let projects_json = serde_json::to_string(
        &scope
            .projects()
            .iter()
            .map(Uuid::to_string)
            .collect::<Vec<_>>(),
    )?;

    // A PM seat that cannot act while its succession is finally refused.
    let mut statement = store
        .conn
        .prepare("SELECT project_id FROM harness_manager_scopes ORDER BY project_id")?;
    let projects: Vec<String> = statement
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    for text in projects {
        let Ok(project_id) = Uuid::parse_str(&text) else {
            continue;
        };
        if !scope.allows(project_id) {
            continue;
        }
        let config = if let Some(seat) = scope.area_seat {
            store
                .manager_area_authority_current(seat)?
                .map(|area| area.config)
        } else {
            store.get_harness_manager_notice_config(project_id)?
        };
        let Some(config) = config else {
            continue;
        };
        let seat = config.current_session_id;
        let seat_status = match seat {
            Some(id) => store.get_session(id)?.map(|session| session.status),
            None => None,
        };
        for decision in store.manager_v2_pending_decision_summaries(&config, None, 50)? {
            if decision.answerable_by != "manager" {
                continue;
            }
            if let Some(asked_at) = parse_time(&decision.created_at) {
                facts
                    .pending_manager_decisions
                    .push(PendingManagerDecision {
                        key: decision.key,
                        project_id,
                        asked_at,
                        execution_id: decision.execution_id,
                    });
            }
        }
        if !config.is_revoked() {
            for cap in store.manager_own_creation_caps(&config)? {
                facts
                    .creation_caps
                    .push(CreationCapFact { project_id, cap });
            }
        }
        let vacant = seat_status.is_none_or(|status| {
            matches!(
                status,
                SessionStatus::Failed
                    | SessionStatus::Completed
                    | SessionStatus::Archived
                    | SessionStatus::Deleted
            )
        });
        if !vacant {
            continue;
        }
        let refusal =
            store.manager_succession_refusal(seat.unwrap_or(config.manager_session_id))?;
        if let Some(refusal) = refusal {
            facts.seats.push(SeatFact {
                project_id,
                seat_status,
                refusal,
            });
        }
    }

    let epics_json = scope
        .epic_ids
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let mut statement = store.conn.prepare(
        "SELECT id, updated_at, json_extract(blocked_reason_json,'$.kind')
         FROM topology_executions
         WHERE status='blocked' AND project_id IN (SELECT value FROM json_each(?1))
           AND (?2 IS NULL OR epic_id IN (SELECT value FROM json_each(?2)))
         ORDER BY updated_at LIMIT 50",
    )?;
    let rows = statement.query_map(params![projects_json, epics_json], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (id, updated_at, kind) = row?;
        if let (Ok(execution_id), Some(blocked_since)) =
            (Uuid::parse_str(&id), parse_time(&updated_at))
        {
            facts.blocked_executions.push(BlockedExecution {
                execution_id,
                blocked_since,
                reason_kind: kind,
            });
        }
    }

    // #2786: an attempt's real wall limit is the earlier of its execution's
    // deadline and the node wall time from its own start plus the time it
    // spent waiting on an answer (`wall_time_expired_after`); an execution
    // with no deadline of its own still bounds every running attempt.
    let mut statement = store.conn.prepare(
        "SELECT a.id, a.execution_id, a.node_id, e.deadline_at, a.started_at, e.created_at
         FROM topology_node_attempts a JOIN topology_executions e ON e.id=a.execution_id
         WHERE a.status='running'
           AND e.status IN ('accepted','running','cancelling')
           AND e.project_id IN (SELECT value FROM json_each(?1))
           AND (?2 IS NULL OR e.epic_id IN (SELECT value FROM json_each(?2))) LIMIT 200",
    )?;
    let rows = statement
        .query_map(params![projects_json, epics_json], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (attempt, execution, node_id, execution_deadline, started, created) in rows {
        let (Ok(attempt_id), Ok(execution_id), Some(created_at)) = (
            Uuid::parse_str(&attempt),
            Uuid::parse_str(&execution),
            parse_time(&created),
        ) else {
            continue;
        };
        let started_at = started
            .as_deref()
            .and_then(parse_time)
            .unwrap_or(created_at);
        let paused = crate::topology::store::node_waited(store, attempt_id)?.total(now);
        let wall_deadline = started_at + crate::topology::executor::SESSION_WALL_TIME + paused;
        let deadline = execution_deadline
            .as_deref()
            .and_then(parse_time)
            .map_or(wall_deadline, |limit| limit.min(wall_deadline));
        if deadline < now {
            facts.overrun_attempts.push(OverrunAttempt {
                attempt_id,
                execution_id,
                node_id,
                deadline,
            });
        }
    }

    facts.hub_provider_faults = local_provider_faults(store, "hub", now)?;
    if scope.area_seat.is_none() {
        facts.red_qa = store
            .untracked_red_bounded_qa_in(Some(scope.projects()))?
            .map(|(sha, finished_at)| RedQa { sha, finished_at });
    }
    Ok(facts)
}

#[cfg(test)]
pub(crate) mod tests;

pub(crate) mod provider_windows;
mod satellite_seats;
