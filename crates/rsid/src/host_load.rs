//! Host-load admission (#1417): hold a manager's new worker launches while the
//! host is too busy to start them.
//!
//! Several projects' managers share one host. Their workers build and test, so
//! a few dozen of them started together push the 1-minute load average far past
//! the core count (53-73 on 32 cores) and make the tests that run on that host
//! flake. The rule used to be "check `uptime`, queue above load 40" mailed to
//! every manager by hand; the daemon now applies it itself.
//!
//! While the host's 1-minute load average plus the launches the daemon admitted
//! in the last minute is above the operator's `host_load_admission_threshold`
//! (default 40; 0 disables the hold), the daemon HOLDS, never refuses:
//!
//! * a manager's new `create_session` action (Issue-worker launches and reviews
//!   included) stays `queued`; `claim_manager_action_with_create_admission` leaves it
//!   unclaimed, exactly as the deploy drain does (#1073), so a restart finds
//!   nothing to mark uncertain. `AgentManagerGetAction` shows it as
//!   `held {reason: host_load, load, threshold}`.
//! * a topology node launched for a manager or Epic lead waits `Reserved`
//!   (`Executor::launch_attempt`) and is retried on the executor's next tick.
//!
//! Host pressure (#2562, `host_pressure`) holds the same work through the same
//! queue: a CPU at or above `job_thermal_ceiling_c` (`held {reason: thermal}`) or
//! free disk below the governor floor (`reason: disk_pressure`) holds it even
//! when the load hold is disabled; a topology node records it as
//! `admission_hold{kind}`.
//!
//! Both start on their own once the load drops. Held work is released
//! oldest-first: the claim query orders queued creates by `(not_before, id)`
//! across every project. Eligible creates and waiting topology nodes share
//! one held queue (`admit_manager_create`, `admit_waiter`) ordered by that
//! due time or execution age, so neither lane overtakes older held work.
//!
//! The 1-minute load average lags: a launch admitted now barely shows for a
//! minute, so releasing the whole backlog the moment the load dips below the
//! threshold would spike it again. Each launch the daemon admitted in the last
//! [`ADMISSION_WINDOW`] therefore counts one toward the comparison
//! (`load + recent_admissions > threshold` holds), and ages out as the
//! average absorbs it.
//!
//! Never held: operator sessions, a worker's own `AgentSpawnChild` children,
//! retries, rotation successors, Issue-worker continuations, topology launches
//! already in progress, lead recovery (`replace_lead`, `retry_lead`,
//! `resume_lead`) and everything else that is not a manager's new worker.
//! They continue work that was already admitted or that the operator asked for,
//! and holding a lead's recovery under load would strand its Epic.
//!
//! The load comes from `/proc/loadavg` on Linux and from `getloadavg(3)` on
//! macOS and other unix systems (#2937). A platform without either (or an
//! unreadable file) reports unsupported and admits everything.

use crate::config::RuntimeConfig;
use crate::host_pressure::PressureHold;
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_daemon_info::{HOST_LOAD, HeldWorkV1, HostLoadAdmissionV1};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long an admitted launch counts toward the load comparison: one
/// 1-minute load-average window.
pub const ADMISSION_WINDOW: Duration = Duration::from_secs(60);

/// Held work that has not asked again for this long is dropped
/// from the queue: it ended, was cancelled or became ineligible, and must not keep
/// younger nodes waiting behind it. The executor asks at least every 30 s.
const WAITER_STALE: Duration = Duration::from_secs(90);

/// How long a sandbox free-space refusal holds new launches before the next one
/// tries again (#2920). The retry itself runs the allocator's one reclaim page,
/// so the hold is a delay between pages, not a permanent block.
pub const SANDBOX_FREE_SPACE_RETRY: Duration = Duration::from_secs(30);

/// Admissions remembered at most (bounds memory while the hold is disabled).
const MAX_RECORDED_ADMISSIONS: usize = 4096;

/// Held launches listed by `AgentGetDaemonInfo` at most.
pub const HELD_LIST_LIMIT: usize = 64;

/// One reading of the host's load.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoadReading {
    /// The 1-minute load average.
    Load1(f64),
    /// This platform reports no load average; everything is admitted.
    Unsupported,
}

/// Where the gate reads the host's load from (swapped by tests).
pub type LoadSource = Arc<dyn Fn() -> LoadReading + Send + Sync>;

/// Read the host's 1-minute load average: `/proc/loadavg` on Linux,
/// `getloadavg(3)` on macOS and other unix systems (#2937).
#[cfg(unix)]
#[must_use]
pub fn read_host_load() -> LoadReading {
    crate::daemon_info::load_average().map_or(LoadReading::Unsupported, |load| {
        LoadReading::Load1(load.one)
    })
}

/// No load average here: admit everything.
#[cfg(not(unix))]
#[must_use]
pub fn read_host_load() -> LoadReading {
    LoadReading::Unsupported
}

/// The source a production daemon uses. Tests (`cfg(test)` and the
/// `test-seam` feature, which never ships) default to unsupported so a loaded
/// CI host cannot hold the manager-action tests; a test that exercises the
/// hold installs its own source.
fn default_source() -> LoadSource {
    #[cfg(any(test, feature = "test-seam"))]
    {
        Arc::new(|| LoadReading::Unsupported)
    }
    #[cfg(not(any(test, feature = "test-seam")))]
    {
        Arc::new(read_host_load)
    }
}

/// Where the gate reads host pressure (thermal, disk) from (swapped by tests).
pub type PressureSource = Arc<dyn Fn() -> Option<PressureHold> + Send + Sync>;

/// The pressure source a production daemon uses: the shared thermal and
/// disk-pressure predicate against the live `min_free_disk_gb` floor (#2562).
/// Tests never see a hot or full CI host: they default to no pressure.
fn default_pressure_source(config: &Arc<RuntimeConfig>) -> PressureSource {
    #[cfg(any(test, feature = "test-seam"))]
    {
        let _ = config;
        Arc::new(|| None)
    }
    #[cfg(not(any(test, feature = "test-seam")))]
    {
        let config = Arc::clone(config);
        Arc::new(move || crate::host_pressure::hold_now(config.governor_policy().min_free_disk_gb))
    }
}

/// Why a launch is held right now: the host is busy ([`HostLoadHold`]) or hot
/// or full ([`PressureHold`], #2562).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AdmissionHold {
    Load(HostLoadHold),
    Pressure(PressureHold),
}

impl AdmissionHold {
    /// The wire `reason`: `host_load`, `thermal`, `disk_pressure` or `sandbox_free_space`.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Load(_) => HOST_LOAD,
            Self::Pressure(pressure) => pressure.reason(),
        }
    }
}

/// Why a launch is held right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostLoadHold {
    /// The host's 1-minute load average.
    pub load: f64,
    /// The operator threshold.
    pub threshold: u32,
    /// Launches admitted in the last [`ADMISSION_WINDOW`].
    pub recent_admissions: u32,
}

struct Waiter {
    kind: &'static str,
    hold: AdmissionHold,
    since: DateTime<Utc>,
    seen: Instant,
    session_id: Option<Uuid>,
}

#[derive(Default)]
struct State {
    admitted: VecDeque<Instant>,
    waiters: HashMap<Uuid, Waiter>,
}

/// One consistent look at the load, the threshold and the recent admissions.
#[derive(Clone, Copy)]
struct Look {
    threshold: u32,
    reading: LoadReading,
    recent: u32,
    pressure: Option<PressureHold>,
}

impl Look {
    /// The load to compare when the hold is enabled: a threshold above 0 and
    /// a platform that reports one.
    fn load(self) -> Option<f64> {
        match self.reading {
            LoadReading::Load1(load) if self.threshold != 0 => Some(load),
            _ => None,
        }
    }

    /// The hold in force: host pressure (thermal, disk) first, then load.
    fn any_hold(self) -> Option<AdmissionHold> {
        self.pressure
            .map(AdmissionHold::Pressure)
            .or_else(|| self.hold().map(AdmissionHold::Load))
    }

    /// The load hold as it reads now (load enabled and supported).
    fn load_hold_now(self) -> Option<AdmissionHold> {
        self.load().map(|load| {
            AdmissionHold::Load(HostLoadHold {
                load,
                threshold: self.threshold,
                recent_admissions: self.recent,
            })
        })
    }

    /// `None` admits: disabled, unsupported, or within the threshold.
    fn hold(self) -> Option<HostLoadHold> {
        let load = self.load()?;
        (load + f64::from(self.recent) > f64::from(self.threshold)).then_some(HostLoadHold {
            load,
            threshold: self.threshold,
            recent_admissions: self.recent,
        })
    }
}

/// Process-wide (per `SessionManager`) host-load admission state.
pub struct HostLoadAdmission {
    config: Arc<RuntimeConfig>,
    source: Mutex<LoadSource>,
    pressure: Mutex<PressureSource>,
    /// The latest sandbox free-space refusal and when it stops holding (#2920).
    sandbox_free_space: Mutex<Option<(PressureHold, Instant)>>,
    state: Mutex<State>,
}

impl HostLoadAdmission {
    #[must_use]
    pub fn new(config: Arc<RuntimeConfig>) -> Self {
        Self::with_source(config, default_source())
    }

    #[must_use]
    pub fn with_source(config: Arc<RuntimeConfig>, source: LoadSource) -> Self {
        let pressure = default_pressure_source(&config);
        Self {
            config,
            source: Mutex::new(source),
            pressure: Mutex::new(pressure),
            sandbox_free_space: Mutex::new(None),
            state: Mutex::new(State::default()),
        }
    }

    /// Replace the pressure source (tests exercising the thermal and disk holds).
    #[cfg(any(test, feature = "test-seam"))]
    pub fn set_pressure_source(&self, source: PressureSource) {
        *self.pressure.lock().unwrap_or_else(PoisonError::into_inner) = source;
    }

    fn read_pressure(&self) -> Option<PressureHold> {
        let source = Arc::clone(&self.pressure.lock().unwrap_or_else(PoisonError::into_inner));
        source().or_else(|| self.sandbox_free_space_hold())
    }

    /// The sandbox allocator refused a launch for free space (#2920): hold every
    /// new launch (manager creates and topology nodes) as `sandbox_free_space`
    /// for [`SANDBOX_FREE_SPACE_RETRY`], then let the next one try again. The
    /// refused work stays queued and starts on its own once space returns.
    pub fn note_sandbox_free_space(&self, available_bytes: u64, min_free_bytes: u64) {
        self.note_sandbox_free_space_for(available_bytes, min_free_bytes, SANDBOX_FREE_SPACE_RETRY);
    }

    fn note_sandbox_free_space_for(
        &self,
        available_bytes: u64,
        min_free_bytes: u64,
        retry: Duration,
    ) {
        const GIB: u64 = 1 << 30;
        let hold = PressureHold::SandboxFreeSpace {
            free_gb: available_bytes as f64 / GIB as f64,
            min_gb: min_free_bytes.div_ceil(GIB),
        };
        *self
            .sandbox_free_space
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((hold, Instant::now() + retry));
    }

    /// Let the sandbox free-space hold end now (tests; production waits out
    /// [`SANDBOX_FREE_SPACE_RETRY`]).
    #[cfg(any(test, feature = "test-seam"))]
    pub fn expire_sandbox_free_space_hold(&self) {
        *self
            .sandbox_free_space
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn sandbox_free_space_hold(&self) -> Option<PressureHold> {
        let mut latch = self
            .sandbox_free_space
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match *latch {
            Some((hold, until)) if Instant::now() < until => Some(hold),
            Some(_) => {
                *latch = None;
                None
            }
            None => None,
        }
    }

    /// The host-pressure hold in force now (#2562): thermal or disk. A launch
    /// that bypasses the load queue (an operator's first launch) still waits
    /// for it.
    #[must_use]
    pub fn pressure_now(&self) -> Option<PressureHold> {
        self.read_pressure()
    }

    /// Replace the load source (tests exercising the hold).
    #[cfg(any(test, feature = "test-seam"))]
    pub fn set_source(&self, source: LoadSource) {
        *self.source.lock().unwrap_or_else(PoisonError::into_inner) = source;
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn threshold(&self) -> u32 {
        self.config
            .host_load_admission_threshold
            .load(Ordering::Relaxed)
    }

    fn read(&self) -> LoadReading {
        let source = Arc::clone(&self.source.lock().unwrap_or_else(PoisonError::into_inner));
        source()
    }

    /// Admissions in the last [`ADMISSION_WINDOW`] (older ones are dropped).
    fn recent_admissions(state: &mut State, now: Instant) -> u32 {
        while state
            .admitted
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= ADMISSION_WINDOW)
        {
            state.admitted.pop_front();
        }
        u32::try_from(state.admitted.len()).unwrap_or(u32::MAX)
    }

    /// Look at the load. A disabled hold (threshold 0) never reads the host.
    fn look(&self, state: &mut State, now: Instant) -> Look {
        let recent = Self::recent_admissions(state, now);
        let threshold = self.threshold();
        Look {
            threshold,
            reading: if threshold == 0 {
                LoadReading::Unsupported
            } else {
                self.read()
            },
            recent,
            pressure: self.read_pressure(),
        }
    }

    fn record_admission(state: &mut State, now: Instant) {
        if state.admitted.len() >= MAX_RECORDED_ADMISSIONS {
            state.admitted.pop_front();
        }
        state.admitted.push_back(now);
    }

    /// The hold in force right now, `None` when a launch is admitted. Decides
    /// whether the manager-action claim leaves queued `create_session`
    /// actions alone and what `AgentManagerGetAction` reports.
    #[must_use]
    pub fn hold_now(&self) -> Option<HostLoadHold> {
        self.hold_at(Instant::now())
    }

    /// Include a queued create's place behind older held work in its receipt.
    #[must_use]
    pub fn hold_for_create(&self, key: Uuid) -> Option<AdmissionHold> {
        let now = Instant::now();
        let mut state = self.state();
        state
            .waiters
            .retain(|_, waiter| now.saturating_duration_since(waiter.seen) < WAITER_STALE);
        let look = self.look(&mut state, now);
        if let Some(hold) = look.any_hold() {
            return Some(hold);
        }
        let waiter = state.waiters.get(&key)?;
        let older = state
            .waiters
            .iter()
            .filter(|(other, older)| (older.since, **other) < (waiter.since, key))
            .min_by_key(|(other, older)| (older.since, **other))
            .map(|(_, older)| older.hold)?;
        Some(look.load_hold_now().unwrap_or(older))
    }

    /// The store calls this only for an eligible new create. It shares the
    /// held queue with topology nodes, so neither lane can consume capacity
    /// indefinitely ahead of older work in the other lane.
    #[must_use]
    pub fn admit_manager_create(
        &self,
        key: Uuid,
        since: DateTime<Utc>,
        session_id: Option<Uuid>,
    ) -> bool {
        self.admit_at(
            key,
            since,
            session_id,
            "manager_create_session",
            Instant::now(),
        )
        .is_none()
    }

    fn hold_at(&self, now: Instant) -> Option<HostLoadHold> {
        let mut state = self.state();
        self.look(&mut state, now).hold()
    }

    /// Record that the daemon admitted one launch (a manager `create_session`
    /// was claimed). It counts toward the load comparison for the next
    /// [`ADMISSION_WINDOW`].
    pub fn note_admitted(&self) {
        self.note_admitted_at(Instant::now());
    }

    fn note_admitted_at(&self, now: Instant) {
        Self::record_admission(&mut self.state(), now);
    }

    /// Admission for work that waits in place of a durable queue (a topology
    /// node): `None` admits and records the admission; `Some` holds, and the
    /// caller asks again on its next tick. A held caller is queued under
    /// `key` by `since` (the age of its execution) and only the oldest queued
    /// caller (including eligible manager creates) is admitted, so held
    /// launches start oldest-first.
    #[must_use]
    pub fn admit_waiter(
        &self,
        key: Uuid,
        since: DateTime<Utc>,
        session_id: Option<Uuid>,
    ) -> Option<AdmissionHold> {
        self.admit_waiter_at(key, since, session_id, Instant::now())
    }

    fn admit_waiter_at(
        &self,
        key: Uuid,
        since: DateTime<Utc>,
        session_id: Option<Uuid>,
        now: Instant,
    ) -> Option<AdmissionHold> {
        self.admit_at(key, since, session_id, "topology_node", now)
    }

    fn admit_at(
        &self,
        key: Uuid,
        since: DateTime<Utc>,
        session_id: Option<Uuid>,
        kind: &'static str,
        now: Instant,
    ) -> Option<AdmissionHold> {
        let mut state = self.state();
        state
            .waiters
            .retain(|_, waiter| now.saturating_duration_since(waiter.seen) < WAITER_STALE);
        let look = self.look(&mut state, now);
        let current = look.any_hold();
        if current.is_none() && look.load().is_none() && state.waiters.is_empty() {
            // Disabled or unsupported and nothing queued: admit.
            Self::record_admission(&mut state, now);
            return None;
        }
        let blocker = state
            .waiters
            .iter()
            .filter(|(other, waiter)| **other != key && (waiter.since, **other) < (since, key))
            .min_by_key(|(other, waiter)| (waiter.since, **other))
            .map(|(_, waiter)| waiter.hold);
        if current.is_none() && blocker.is_none() {
            state.waiters.remove(&key);
            Self::record_admission(&mut state, now);
            return None;
        }
        let hold = current
            .or_else(|| look.load_hold_now())
            .or(blocker)
            .expect("a current hold, the load, or an older waiter explains the hold");
        state.waiters.insert(
            key,
            Waiter {
                kind,
                hold,
                since,
                seen: now,
                session_id,
            },
        );
        Some(hold)
    }

    /// `AgentGetDaemonInfo` status. `queued_creates` are the due, queued
    /// manager `create_session` actions oldest first (`(target session,
    /// RFC3339 not_before)`); they are listed as held only while the hold is
    /// in force.
    #[must_use]
    pub fn status(&self, queued_creates: &[(Option<Uuid>, String)]) -> HostLoadAdmissionV1 {
        let now = Instant::now();
        let mut state = self.state();
        state
            .waiters
            .retain(|_, waiter| now.saturating_duration_since(waiter.seen) < WAITER_STALE);
        let recent = Self::recent_admissions(&mut state, now);
        let threshold = self.threshold();
        let reading = self.read();
        let look = Look {
            threshold,
            reading,
            recent,
            pressure: self.read_pressure(),
        };
        let current = look.any_hold();
        let holding = current.is_some();
        let creates_reason = current.map_or(HOST_LOAD, AdmissionHold::reason);
        let mut held: Vec<(String, HeldWorkV1)> = Vec::new();
        if holding {
            for (target, since) in queued_creates {
                held.push((
                    since.clone(),
                    HeldWorkV1 {
                        kind: "manager_create_session".to_string(),
                        session_id: *target,
                        since: since.clone(),
                        reason: creates_reason.to_string(),
                    },
                ));
            }
        }
        for waiter in state.waiters.values() {
            if waiter.kind == "manager_create_session"
                && holding
                && queued_creates
                    .iter()
                    .any(|(target, _)| *target == waiter.session_id)
            {
                continue;
            }
            let since = waiter.since.to_rfc3339_opts(SecondsFormat::Nanos, true);
            held.push((
                since.clone(),
                HeldWorkV1 {
                    kind: waiter.kind.to_string(),
                    session_id: waiter.session_id,
                    since,
                    reason: waiter.hold.reason().to_string(),
                },
            ));
        }
        held.sort_by(|a, b| a.0.cmp(&b.0));
        held.truncate(HELD_LIST_LIMIT);
        HostLoadAdmissionV1 {
            threshold,
            supported: matches!(reading, LoadReading::Load1(_)),
            load: match reading {
                LoadReading::Load1(load) => Some(load),
                LoadReading::Unsupported => None,
            },
            recent_admissions: recent,
            holding,
            held: held.into_iter().map(|(_, item)| item).collect(),
            build_disk: Some(crate::host_pressure::build_disk_floor(
                crate::host_pressure::disk_free_gb(),
                self.config.governor_policy().min_free_disk_gb,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::atomic::AtomicU64;

    /// A gate whose load the test sets, with the threshold 40 default.
    struct Fixture {
        gate: HostLoadAdmission,
        load: Arc<Mutex<LoadReading>>,
        config: Arc<RuntimeConfig>,
    }

    fn fixture(load: LoadReading) -> Fixture {
        let config = RuntimeConfig::from_config(&Config::default());
        let load = Arc::new(Mutex::new(load));
        let shared = Arc::clone(&load);
        let gate = HostLoadAdmission::with_source(
            Arc::clone(&config),
            Arc::new(move || *shared.lock().unwrap()),
        );
        Fixture { gate, load, config }
    }

    impl Fixture {
        fn set_load(&self, load: f64) {
            *self.load.lock().unwrap() = LoadReading::Load1(load);
        }

        fn set_threshold(&self, threshold: u32) {
            self.config
                .update_field(
                    "host_load_admission_threshold",
                    &serde_json::json!(threshold),
                )
                .unwrap();
        }
    }

    /// Install a pressure source the test steers; `None` is a cool, roomy host.
    fn steer_pressure(f: &Fixture) -> Arc<Mutex<Option<PressureHold>>> {
        let shared = Arc::new(Mutex::new(None));
        let source = Arc::clone(&shared);
        f.gate
            .set_pressure_source(Arc::new(move || *source.lock().unwrap()));
        shared
    }

    fn hot() -> PressureHold {
        PressureHold::Thermal(rsid_core::thermal_guard::ThermalHold {
            temperature_c: 93.0,
            ceiling_c: 85,
        })
    }

    fn full_disk() -> PressureHold {
        PressureHold::DiskPressure {
            free_gb: 12.0,
            min_gb: 30,
        }
    }

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn since(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn holds_above_the_threshold_and_admits_at_or_below_it() {
        let f = fixture(LoadReading::Load1(40.0));
        let now = Instant::now();
        assert_eq!(f.gate.hold_at(now), None, "the threshold itself admits");
        f.set_load(40.5);
        let hold = f.gate.hold_at(now).expect("above the threshold holds");
        assert_eq!(hold.threshold, 40);
        assert!((hold.load - 40.5).abs() < f64::EPSILON);
        assert_eq!(hold.recent_admissions, 0);
        f.set_load(12.0);
        assert_eq!(f.gate.hold_at(now), None, "released once the load drops");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_zero_threshold_disables_the_hold_without_reading_the_host() {
        let f = fixture(LoadReading::Load1(90.0));
        assert!(f.gate.hold_now().is_some());
        f.set_threshold(0);
        assert_eq!(f.gate.hold_now(), None);
        let calls = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&calls);
        f.gate.set_source(Arc::new(move || {
            counted.fetch_add(1, Ordering::Relaxed);
            LoadReading::Load1(90.0)
        }));
        assert_eq!(f.gate.hold_now(), None);
        assert_eq!(calls.load(Ordering::Relaxed), 0, "disabled never reads");
        assert_eq!(
            f.gate.admit_waiter(Uuid::new_v4(), since(0), None),
            None,
            "a disabled hold admits every waiter"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_threshold_applies_live() {
        let f = fixture(LoadReading::Load1(30.0));
        assert_eq!(f.gate.hold_now(), None);
        f.set_threshold(25);
        assert_eq!(f.gate.hold_now().map(|hold| hold.threshold), Some(25));
        f.set_threshold(60);
        assert_eq!(f.gate.hold_now(), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn an_unsupported_platform_admits_everything() {
        let f = fixture(LoadReading::Unsupported);
        assert_eq!(f.gate.hold_now(), None);
        assert_eq!(f.gate.admit_waiter(Uuid::new_v4(), since(0), None), None);
        let status = f
            .gate
            .status(&[(Some(Uuid::new_v4()), "2026-10-07T00:00:00Z".into())]);
        assert!(!status.supported);
        assert_eq!(status.load, None);
        assert!(!status.holding);
        assert!(status.held.is_empty());
        assert_eq!(status.threshold, 40);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recent_admissions_count_toward_the_load_until_the_window_passes() {
        let f = fixture(LoadReading::Load1(38.5));
        let base = Instant::now();
        assert_eq!(f.gate.hold_at(base), None);
        f.gate.note_admitted_at(base);
        assert_eq!(f.gate.hold_at(base), None, "38.5 + 1 is within 40");
        f.gate.note_admitted_at(at(base, 1));
        let hold = f.gate.hold_at(at(base, 2)).expect("38.5 + 2 is above 40");
        assert_eq!(hold.recent_admissions, 2);
        assert_eq!(
            f.gate.hold_at(at(base, 61)),
            None,
            "the first admission aged out, the second still counts one"
        );
        assert_eq!(f.gate.hold_at(at(base, 62)), None);
    }

    /// #163: daemon info names the live build disk floor before any dispatch,
    /// whatever the launch hold is doing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn status_reports_the_live_build_disk_floor_beside_an_open_launch_gate() {
        let f = fixture(LoadReading::Load1(1.0));
        let floor = f.gate.status(&[]).build_disk.expect("a floor is reported");
        assert_eq!(floor.min_free_gb, 30, "the governor default");
        f.config
            .governor_min_free_disk_gb
            .store(41, Ordering::Relaxed);
        let status = f.gate.status(&[]);
        assert!(!status.holding, "an idle host holds no launch");
        let floor = status.build_disk.expect("a floor is reported");
        assert_eq!(floor.min_free_gb, 41, "the live setting, not a default");
        assert_eq!(
            floor.open,
            floor.free_gb.is_none_or(|free| free >= 41.0),
            "open follows the reported margin"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn held_waiters_are_released_oldest_first() {
        let f = fixture(LoadReading::Load1(55.0));
        let base = Instant::now();
        let (oldest, middle, youngest) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        // Asking in the opposite order of age: all are held.
        for (key, age) in [(youngest, 30), (middle, 20), (oldest, 10)] {
            assert!(
                f.gate
                    .admit_waiter_at(key, since(age), Some(key), at(base, 0))
                    .is_some(),
                "{key} is held above the threshold"
            );
        }
        f.set_load(10.0);
        // The load dropped: the youngest asks first and is still held.
        assert!(
            f.gate
                .admit_waiter_at(youngest, since(30), None, at(base, 10))
                .is_some(),
            "a younger waiter never overtakes an older one"
        );
        assert!(
            f.gate
                .admit_waiter_at(middle, since(20), None, at(base, 10))
                .is_some()
        );
        assert_eq!(
            f.gate
                .admit_waiter_at(oldest, since(10), None, at(base, 10)),
            None,
            "the oldest waiter goes first"
        );
        assert_eq!(
            f.gate
                .admit_waiter_at(middle, since(20), None, at(base, 11)),
            None,
            "then the next oldest"
        );
        assert_eq!(
            f.gate
                .admit_waiter_at(youngest, since(30), None, at(base, 12)),
            None
        );
        assert!(f.gate.status(&[]).held.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn manager_and_topology_waiters_share_oldest_first_admission() {
        let f = fixture(LoadReading::Load1(75.0));
        let now = Instant::now();
        let (manager, topology) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(
            f.gate
                .admit_at(
                    manager,
                    since(0),
                    Some(manager),
                    "manager_create_session",
                    now
                )
                .is_some()
        );
        assert!(
            f.gate
                .admit_waiter_at(topology, since(1), Some(topology), now)
                .is_some()
        );
        f.set_load(10.0);
        for _ in 0..3 {
            assert!(
                f.gate
                    .admit_waiter_at(topology, since(1), Some(topology), now)
                    .is_some()
            );
        }
        assert!(
            f.gate
                .admit_manager_create(manager, since(0), Some(manager))
        );
        assert_eq!(
            f.gate
                .admit_waiter_at(topology, since(1), Some(topology), now),
            None
        );
        assert_eq!(f.gate.status(&[]).recent_admissions, 2);
        assert!(f.gate.status(&[]).held.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn recent_admissions_are_bounded_and_expire_even_after_disabled_launches() {
        let f = fixture(LoadReading::Load1(10.0));
        f.set_threshold(0);
        let now = Instant::now();
        for _ in 0..MAX_RECORDED_ADMISSIONS + 10 {
            assert_eq!(
                f.gate.admit_waiter_at(Uuid::new_v4(), since(0), None, now),
                None
            );
        }
        assert_eq!(f.gate.state().admitted.len(), MAX_RECORDED_ADMISSIONS);
        f.set_threshold(40);
        assert!(f.gate.hold_at(now).is_some());
        assert_eq!(f.gate.hold_at(now + ADMISSION_WINDOW), None);
        assert!(f.gate.state().admitted.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_vanished_waiter_stops_blocking_younger_ones_after_the_stale_window() {
        let f = fixture(LoadReading::Load1(55.0));
        let base = Instant::now();
        let (gone, waiting) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(f.gate.admit_waiter_at(gone, since(0), None, base).is_some());
        assert!(
            f.gate
                .admit_waiter_at(waiting, since(5), None, base)
                .is_some()
        );
        f.set_load(5.0);
        assert!(
            f.gate
                .admit_waiter_at(waiting, since(5), None, at(base, 30))
                .is_some(),
            "the older waiter is still queued"
        );
        assert_eq!(
            f.gate
                .admit_waiter_at(waiting, since(5), None, at(base, 100)),
            None,
            "its execution never asked again, so it no longer blocks"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn status_reports_the_hold_and_lists_held_work_oldest_first() {
        let f = fixture(LoadReading::Load1(52.9));
        let (create_a, create_b, node) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        assert!(
            f.gate
                .admit_waiter(Uuid::new_v4(), since(5), Some(node))
                .is_some()
        );
        let queued = vec![
            (
                Some(create_a),
                since(1).to_rfc3339_opts(SecondsFormat::Nanos, true),
            ),
            (
                Some(create_b),
                since(9).to_rfc3339_opts(SecondsFormat::Nanos, true),
            ),
        ];
        let status = f.gate.status(&queued);
        assert!(status.supported && status.holding);
        assert_eq!(status.threshold, 40);
        assert!((status.load.unwrap() - 52.9).abs() < f64::EPSILON);
        let order: Vec<(&str, Option<Uuid>)> = status
            .held
            .iter()
            .map(|item| (item.kind.as_str(), item.session_id))
            .collect();
        assert_eq!(
            order,
            vec![
                ("manager_create_session", Some(create_a)),
                ("topology_node", Some(node)),
                ("manager_create_session", Some(create_b)),
            ]
        );
        assert!(status.held.iter().all(|item| item.reason == HOST_LOAD));
        f.set_load(8.0);
        let released = f.gate.status(&queued);
        assert!(!released.holding);
        assert!(
            released
                .held
                .iter()
                .all(|item| item.kind == "topology_node"),
            "queued creates are listed only while the hold is in force"
        );
    }

    /// #2920: a sandbox free-space refusal holds every launch as
    /// `sandbox_free_space` (also topology waiters) until its retry delay ends;
    /// a real pressure reading outranks it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_sandbox_free_space_refusal_holds_launches_until_the_retry_delay_ends() {
        let f = fixture(LoadReading::Load1(1.0));
        f.set_threshold(0);
        let pressure = steer_pressure(&f);
        let node = Uuid::new_v4();
        let gib = 1_u64 << 30;
        f.gate.note_sandbox_free_space(8 * gib, 30 * gib);
        let hold = f
            .gate
            .admit_waiter(node, since(5), Some(node))
            .expect("a free-space refusal holds");
        assert_eq!(hold.reason(), crate::host_pressure::SANDBOX_FREE_SPACE);
        assert_eq!(
            hold,
            AdmissionHold::Pressure(PressureHold::SandboxFreeSpace {
                free_gb: 8.0,
                min_gb: 30
            })
        );
        *pressure.lock().unwrap() = Some(hot());
        assert_eq!(f.gate.pressure_now(), Some(hot()), "real pressure first");
        *pressure.lock().unwrap() = None;
        f.gate.expire_sandbox_free_space_hold();
        assert!(f.gate.admit_waiter(node, since(5), Some(node)).is_none());
        // The delay lapses by itself.
        f.gate
            .note_sandbox_free_space_for(8 * gib, 30 * gib, Duration::ZERO);
        assert_eq!(f.gate.pressure_now(), None);
    }

    /// #2562: a hot or full host holds launches even with an idle load and the
    /// load hold disabled, and releases them oldest-first once it clears.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn pressure_holds_waiters_with_an_idle_load_and_releases_them_oldest_first() {
        let f = fixture(LoadReading::Load1(1.0));
        f.set_threshold(0);
        let pressure = steer_pressure(&f);
        let base = Instant::now();
        let (older, younger) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(
            f.gate
                .admit_waiter_at(younger, since(30), Some(younger), at(base, 0)),
            None,
            "a cool host with nothing queued admits"
        );
        *pressure.lock().unwrap() = Some(hot());
        for (key, age) in [(younger, 30), (older, 10)] {
            let hold = f
                .gate
                .admit_waiter_at(key, since(age), Some(key), at(base, 1))
                .expect("a hot host holds");
            assert_eq!(hold.reason(), crate::host_pressure::THERMAL);
        }
        let status = f.gate.status(&[]);
        assert!(status.holding);
        assert_eq!(
            status
                .held
                .iter()
                .map(|w| w.reason.as_str())
                .collect::<Vec<_>>(),
            vec!["thermal", "thermal"]
        );
        // Another kind of pressure is reported as its own reason.
        *pressure.lock().unwrap() = Some(full_disk());
        assert_eq!(
            f.gate
                .admit_waiter_at(older, since(10), Some(older), at(base, 2))
                .map(AdmissionHold::reason),
            Some("disk_pressure")
        );
        // The temperature drops and the disk is freed: the oldest goes first.
        *pressure.lock().unwrap() = None;
        assert!(
            f.gate
                .admit_waiter_at(younger, since(30), Some(younger), at(base, 3))
                .is_some(),
            "the younger waiter does not overtake the older one"
        );
        assert_eq!(
            f.gate
                .admit_waiter_at(older, since(10), Some(older), at(base, 3)),
            None
        );
        assert_eq!(
            f.gate
                .admit_waiter_at(younger, since(30), Some(younger), at(base, 4)),
            None
        );
        assert!(f.gate.status(&[]).held.is_empty());
    }

    /// #2562: the manager-action view names the pressure that holds a create.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_queued_create_reports_the_pressure_hold_and_the_claim_admits_after_it() {
        let f = fixture(LoadReading::Unsupported);
        let pressure = steer_pressure(&f);
        let key = Uuid::new_v4();
        assert_eq!(f.gate.hold_for_create(key), None);
        *pressure.lock().unwrap() = Some(hot());
        assert_eq!(
            f.gate.hold_for_create(key),
            Some(AdmissionHold::Pressure(hot()))
        );
        assert!(!f.gate.admit_manager_create(key, since(0), None));
        let queued = [(None, "2026-10-07T00:00:00Z".to_string())];
        let status = f.gate.status(&queued);
        assert_eq!(status.held[0].reason, "thermal");
        *pressure.lock().unwrap() = None;
        assert!(f.gate.admit_manager_create(key, since(0), None));
        assert_eq!(f.gate.hold_for_create(key), None);
    }
}
