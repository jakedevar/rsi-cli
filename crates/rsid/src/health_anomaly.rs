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

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};
use rsi_common::health_anomaly::{
    CONTEXT_FULL_GRACE_SECS, HEALTH_MAX_CONDITIONS, HealthAnomalySpec, HealthCondition,
    KIND_ATTEMPT_PAST_WALL_LIMIT, KIND_CPU_HOT, KIND_EXECUTION_BLOCKED, KIND_HUB_DISK_HIGH,
    KIND_PM_CONTEXT_FULL, KIND_PM_SEAT_BLOCKED, KIND_PROVIDER_AUTH_FAILURE,
    KIND_PROVIDER_MODEL_UNAVAILABLE, KIND_RECLAIM_UNSUPPORTED, KIND_ROLLING_QA_RED,
    KIND_SATELLITE_DISK_LOW, THERMAL_SUSTAIN_SECS,
};
use rsi_common::satellite::{
    SATELLITE_FAULT_AUTH, SATELLITE_FAULT_MODEL_UNAVAILABLE, SATELLITE_MAX_PROVIDER_FAULTS,
    SatelliteHealthV1, SatelliteProviderFaultV1,
};
use rsi_common::types::SessionStatus;
use rsid_core::health_thresholds::HealthThresholds;
use rusqlite::params;
use uuid::Uuid;

use crate::error::Result;
use crate::store::Store;

/// What a wake may observe (#2788): the projects the owner's grant covers
/// right now. Project-bound facts (seats, blocked executions, overrun
/// attempts, a red QA, a PM's context) are limited to `projects`; the host's
/// own readings and the satellites are fleet-wide and only a portfolio seat
/// (`fleet_wide`) hears about the satellites.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scope {
    projects: Vec<Uuid>,
    fleet_wide: bool,
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
        }
    }

    /// One project manager's own project.
    #[must_use]
    pub fn project(project: Uuid) -> Self {
        Self {
            projects: vec![project],
            fleet_wide: false,
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

    /// The scope `owner` may observe now for the stored `spec`: the stored
    /// projects intersected with the owner's current portfolio grant, so a
    /// narrowed or revoked grant narrows the wake and a seat that never held
    /// the project never sees it. A project manager keeps its stored project
    /// only while that is still its session's project. A spec armed before
    /// `projects` existed (portfolio, nothing stored) follows the live grant.
    ///
    /// # Errors
    /// A persistence error.
    pub fn live(store: &Store, owner: Option<Uuid>, spec: &HealthAnomalySpec) -> Result<Self> {
        let Some(owner) = owner else {
            return Ok(Self::none());
        };
        let grant = store.portfolio_seat_grant(owner)?;
        if let Some(only) = spec.project {
            let covered = match &grant {
                Some(grant) => grant.project_ids.contains(&only),
                None => store
                    .get_session(owner)?
                    .is_some_and(|session| session.project_id == Some(only)),
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreFacts {
    pub seats: Vec<SeatFact>,
    pub blocked_executions: Vec<BlockedExecution>,
    pub overrun_attempts: Vec<OverrunAttempt>,
    pub hub_provider_faults: Vec<ProviderFault>,
    pub red_qa: Option<RedQa>,
}

/// This host's disk and CPU now; `None` = unreadable (nothing fires).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostReading {
    pub disk_used_pct: Option<f64>,
    pub cpu_c: Option<f64>,
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
    pub provider_faults: Vec<SatelliteProviderFaultV1>,
}

impl SatelliteReading {
    /// A reading from the health a satellite reported.
    #[must_use]
    pub fn from_health(peer_id: Uuid, label: &str, health: &SatelliteHealthV1) -> Self {
        let admission = health.qa_admission.as_ref();
        Self {
            peer_id,
            label: label.to_owned(),
            disk_free_bytes: admission
                .and_then(|a| a.build_disk_free_bytes)
                .or(health.disk_free_bytes),
            disk_floor_bytes: admission.map(|a| a.build_disk_floor_bytes),
            provider_faults: health.provider_faults.clone(),
        }
    }
}

/// What only the live session manager can read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiveHealthInputs {
    pub pm_contexts: Vec<PmContext>,
    pub satellites: Vec<SatelliteReading>,
}

/// Everything [`evaluate`] looks at.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub store: StoreFacts,
    pub host: HostReading,
    pub live: LiveHealthInputs,
}

/// "Held for N seconds" bookkeeping: the first time each condition key was
/// seen active. An inactive key is forgotten, so a flap restarts the clock.
#[derive(Debug, Default)]
pub struct Sustain {
    first_seen: HashMap<String, DateTime<Utc>>,
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

fn pct(value: f64) -> String {
    format!("{value:.1}%")
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
        found.push(HealthCondition::new(
            KIND_HUB_DISK_HIGH,
            "/",
            format!("used={} threshold={}%", pct(used), thresholds.disk_used_pct),
        ));
    }

    let hot = thresholds.thermal_c > 0
        && inputs
            .host
            .cpu_c
            .is_some_and(|c| c >= f64::from(thresholds.thermal_c));
    if sustain.held(KIND_CPU_HOT, hot, now, THERMAL_SUSTAIN_SECS) {
        found.push(HealthCondition::new(
            KIND_CPU_HOT,
            "hub",
            format!(
                "tctl={:.1}C threshold={}C sustained={}s",
                inputs.host.cpu_c.unwrap_or_default(),
                thresholds.thermal_c,
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

    for satellite in &inputs.live.satellites {
        if let (Some(free), Some(floor)) = (satellite.disk_free_bytes, satellite.disk_floor_bytes)
            && free < floor
        {
            found.push(HealthCondition::new(
                KIND_SATELLITE_DISK_LOW,
                satellite.peer_id.to_string(),
                format!(
                    "label={} free_bytes={free} floor_bytes={floor}",
                    satellite.label
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

    found.retain(|condition| !spec.ignores(condition));
    found.truncate(HEALTH_MAX_CONDITIONS);
    found
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
        let Some(config) = store.get_harness_manager_notice_config(project_id)? else {
            continue;
        };
        let seat = config.current_session_id;
        let seat_status = match seat {
            Some(id) => store.get_session(id)?.map(|session| session.status),
            None => None,
        };
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

    let mut statement = store.conn.prepare(
        "SELECT id, updated_at, json_extract(blocked_reason_json,'$.kind')
         FROM topology_executions
         WHERE status='blocked' AND project_id IN (SELECT value FROM json_each(?1))
         ORDER BY updated_at LIMIT 50",
    )?;
    let rows = statement.query_map(params![projects_json], |row| {
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
           AND e.project_id IN (SELECT value FROM json_each(?1)) LIMIT 200",
    )?;
    let rows = statement
        .query_map(params![projects_json], |row| {
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
    facts.red_qa = store
        .untracked_red_bounded_qa_in(Some(scope.projects()))?
        .map(|(sha, finished_at)| RedQa { sha, finished_at });
    Ok(facts)
}

#[cfg(test)]
pub(crate) mod tests;
