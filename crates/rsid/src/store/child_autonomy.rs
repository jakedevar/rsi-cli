//! Read-mostly store queries behind the child-aware continuation policy
//! (#794 S3): which children a parent is waiting on, whether a wake is held,
//! and the per-window keep-alive row.
//!
//! No schema of its own. The children a parent waits on are its enabled
//! automatic `agent-child-*` terminal watches (armed at every spawn); the
//! keep-alive valve is an ordinary one-shot same-session `Resume`
//! `scheduled_jobs` row whose primary key is derived from
//! `(parent, window_start)`, so a window can have at most one row.

use super::Store;
use crate::error::Result;
use crate::session::harness::tools::schedule_wake::{
    deterministic_program_guard_job_id, is_program_guard_sentinel,
};
use chrono::{DateTime, Duration, Utc};
use rsi_common::child_autonomy::{
    CHILD_WATCH_NAME_PREFIX, KEEPALIVE_ENABLED_DEFAULT, KEEPALIVE_ENABLED_SETTING,
    KEEPALIVE_NAME_PREFIX, KEEPALIVE_WINDOW_DEFAULT_SECS, KEEPALIVE_WINDOW_MAX_SECS,
    KEEPALIVE_WINDOW_MIN_SECS, KEEPALIVE_WINDOW_SETTING, PROGRAM_HOLD_DEFAULT,
    PROGRAM_HOLD_SETTING, ScheduledJobHoldV1,
};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use rusqlite::params;
use uuid::Uuid;

/// The operator settings, read from the `daemon_settings` authority row with
/// the same defaults `RuntimeConfig` publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChildAutonomyPolicy {
    pub hold_program_wakes: bool,
    pub keepalive_enabled: bool,
    pub window: Duration,
}

impl Default for ChildAutonomyPolicy {
    fn default() -> Self {
        Self {
            hold_program_wakes: PROGRAM_HOLD_DEFAULT,
            keepalive_enabled: KEEPALIVE_ENABLED_DEFAULT,
            window: Duration::seconds(KEEPALIVE_WINDOW_DEFAULT_SECS as i64),
        }
    }
}

/// Name of the deterministic valve rows of `parent`.
pub(crate) fn keepalive_row_name(parent: Uuid) -> String {
    format!("{KEEPALIVE_NAME_PREFIX}{parent}")
}

/// Deterministic primary key of the valve row for one window.
pub(crate) fn keepalive_row_id(parent: Uuid, window_start: DateTime<Utc>) -> Uuid {
    let namespace = Uuid::from_u128(0x6b1f0c7e_5d24_4a8b_9c31_7e2a4d90f5b3);
    let key = format!(
        "{parent}:{}",
        window_start.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    );
    Uuid::new_v5(&namespace, key.as_bytes())
}

/// A daemon-owned valve row: exact name, one-shot same-session Resume.
pub(crate) fn is_keepalive_row(job: &ScheduledJob) -> bool {
    job.wake_mode == WakeMode::Resume
        && matches!(job.schedule.recurrence, Recurrence::Once)
        && job
            .wake_session_id
            .is_some_and(|parent| job.name == keepalive_row_name(parent))
}

/// One child a parent is still waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunningChild {
    /// The logical child the watch names (the watched id).
    pub watched: Uuid,
    /// Its published rotation tip, which is what actually runs.
    pub tip: Uuid,
}

impl Store {
    /// Policy for this tick. A read error yields "hold off, valve off" so an
    /// unreadable setting can only ever fall back to the pre-#794 behaviour.
    pub(crate) fn child_autonomy_policy(&self) -> ChildAutonomyPolicy {
        let read = |key: &str| self.get_daemon_setting(key);
        let mut policy = ChildAutonomyPolicy::default();
        let parse_bool = |raw: Option<String>, default: bool| match raw.as_deref() {
            Some("true") => Some(true),
            Some("false") => Some(false),
            None => Some(default),
            _ => None,
        };
        let hold = read(PROGRAM_HOLD_SETTING).map(|raw| parse_bool(raw, PROGRAM_HOLD_DEFAULT));
        let valve =
            read(KEEPALIVE_ENABLED_SETTING).map(|raw| parse_bool(raw, KEEPALIVE_ENABLED_DEFAULT));
        let window = read(KEEPALIVE_WINDOW_SETTING).map(|raw| match raw {
            None => Some(KEEPALIVE_WINDOW_DEFAULT_SECS),
            Some(text) => text.trim().parse::<u64>().ok().filter(|secs| {
                (KEEPALIVE_WINDOW_MIN_SECS..=KEEPALIVE_WINDOW_MAX_SECS).contains(secs)
            }),
        });
        match (hold, valve, window) {
            (Ok(Some(hold)), Ok(Some(valve)), Ok(Some(window))) => {
                policy.hold_program_wakes = hold;
                policy.keepalive_enabled = valve;
                policy.window = Duration::seconds(window as i64);
            }
            _ => {
                tracing::warn!("child autonomy settings unreadable; hold and valve are off");
                policy.hold_program_wakes = false;
                policy.keepalive_enabled = false;
            }
        }
        policy
    }

    /// Children `parent` waits on that are still running: the enabled
    /// automatic child watches whose logical child's published tip is live.
    pub(crate) fn running_children_of(&self, parent: Uuid) -> Result<Vec<RunningChild>> {
        let mut statement = self.conn.prepare(
            "SELECT wake_mode FROM scheduled_jobs \
             WHERE enabled=1 AND wake_session_id=?1 AND wake_mode LIKE 'on_terminal:%' \
               AND name LIKE ?2 ORDER BY created_at, id",
        )?;
        let modes: Vec<String> = statement
            .query_map(
                params![parent.to_string(), format!("{CHILD_WATCH_NAME_PREFIX}%")],
                |row| row.get(0),
            )?
            .collect::<std::result::Result<_, _>>()?;
        let mut running = Vec::new();
        for mode in modes {
            let Some(watched) = mode
                .strip_prefix("on_terminal:")
                .and_then(|id| Uuid::parse_str(id).ok())
            else {
                continue;
            };
            if watched == parent {
                continue;
            }
            let tip = self.published_lineage_tip(watched)?.unwrap_or(watched);
            let Some(child) = self.get_session(tip)? else {
                continue;
            };
            if crate::store_support::wake_target::is_live_wake_target(child.status) {
                running.push(RunningChild { watched, tip });
            }
        }
        Ok(running)
    }

    /// True while `parent` holds an enabled program-guard sentinel.
    pub(crate) fn program_guard_enabled(&self, parent: Uuid) -> Result<bool> {
        Ok(self
            .get_scheduled_job(&deterministic_program_guard_job_id(parent))?
            .is_some_and(|job| job.enabled && is_program_guard_sentinel(&job, parent)))
    }

    /// The parent's idle start: its tip's last provider output, else the
    /// tip row's last update.
    fn idle_since(&self, tip: Uuid) -> Result<Option<DateTime<Utc>>> {
        if let Some(at) = self.last_provider_output_at(tip)? {
            return Ok(Some(at));
        }
        Ok(self.get_session(tip)?.map(|session| session.updated_at))
    }

    /// The hold that applies to `job`, if any. Pure over durable state: the
    /// hold survives a daemon restart because `release_at` derives from the
    /// wake's due time and the parent's last output.
    ///
    /// Applies only to a program-mode master's ordinary one-shot same-session
    /// Resume wake: never the sentinel, never a valve row, never a recurring
    /// or non-program wake.
    pub(crate) fn hold_for_job(
        &self,
        job: &ScheduledJob,
        now: DateTime<Utc>,
        policy: &ChildAutonomyPolicy,
    ) -> Result<Option<ScheduledJobHoldV1>> {
        if !policy.hold_program_wakes
            || job.wake_mode != WakeMode::Resume
            || !matches!(job.schedule.recurrence, Recurrence::Once)
            || is_keepalive_row(job)
        {
            return Ok(None);
        }
        let Some(parent) = job.wake_session_id else {
            return Ok(None);
        };
        if job.id == deterministic_program_guard_job_id(parent)
            || !self.program_guard_enabled(parent)?
        {
            return Ok(None);
        }
        let running = self.running_children_of(parent)?;
        if running.is_empty() {
            return Ok(None);
        }
        let tip = self.published_lineage_tip(parent)?.unwrap_or(parent);
        // Only an idle parent has a held wake; a Failed or Interrupted master
        // belongs to the retry and recovery owners, a running one refuses busy.
        if self
            .get_session(tip)?
            .is_none_or(|session| session.status != rsi_common::types::SessionStatus::Completed)
        {
            return Ok(None);
        }
        let held_since = match self.idle_since(tip)? {
            Some(idle) => idle.max(job.next_fire_at),
            None => job.next_fire_at,
        };
        let release_at = held_since + policy.window;
        if now >= release_at {
            return Ok(None);
        }
        Ok(Some(ScheduledJobHoldV1 {
            job_id: job.id,
            parent_session_id: parent,
            running_children: running.into_iter().map(|child| child.tip).collect(),
            held_since,
            release_at,
        }))
    }

    /// Every currently held wake, for the operator read side.
    pub(crate) fn list_scheduled_job_holds(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<ScheduledJobHoldV1>> {
        let policy = self.child_autonomy_policy();
        let mut holds = Vec::new();
        for job in self.list_scheduled_jobs()? {
            // Only due, enabled rows can be held; a future wake is just pending.
            if !job.enabled || job.next_fire_at > now {
                continue;
            }
            if let Some(hold) = self.hold_for_job(&job, now, &policy)? {
                holds.push(hold);
            }
        }
        Ok(holds)
    }

    /// Parents that have at least one enabled automatic child watch.
    fn keepalive_candidates(&self) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT wake_session_id FROM scheduled_jobs \
             WHERE enabled=1 AND wake_mode LIKE 'on_terminal:%' AND name LIKE ?1 \
               AND wake_session_id IS NOT NULL ORDER BY wake_session_id",
        )?;
        let parents = statement
            .query_map([format!("{CHILD_WATCH_NAME_PREFIX}%")], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(parents
            .into_iter()
            .filter_map(|id| Uuid::parse_str(&id).ok())
            .collect())
    }

    /// Whether `parent`'s tip is idle and unowned, so the valve may act:
    /// `Completed`, not archiving, no pending question/approval/operator or
    /// manager pause/capacity incident, and no enabled Resume wake of any
    /// kind other than the program sentinel (a hand-armed wake, a held wake or
    /// an existing valve row already owns the continuation).
    fn keepalive_eligible(&self, parent: Uuid, tip: Uuid) -> Result<bool> {
        let Some(session) = self.get_session(tip)? else {
            return Ok(false);
        };
        if session.status != rsi_common::types::SessionStatus::Completed || session.pending_archive
        {
            return Ok(false);
        }
        let sentinel = deterministic_program_guard_job_id(parent);
        let blocked: bool = self.conn.query_row(
            // The raw marker, not the hydrated question: hydration tolerates
            // malformed JSON, which must not erase a human gate.
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND pending_question_json IS NOT NULL)
                 OR EXISTS(SELECT 1 FROM approvals WHERE session_id=?1 AND status='Pending')
                 OR EXISTS(SELECT 1 FROM daemon_settings WHERE key=?2)
                 OR EXISTS(SELECT 1 FROM scheduled_jobs
                           WHERE enabled=1 AND wake_mode='resume'
                             AND wake_session_id IN (?3,?4) AND id<>?5)
                 OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents
                           WHERE state='open' AND controller_session_id IN (?3,?4))",
            params![
                tip.to_string(),
                format!("manager_operator_pause:{tip}"),
                parent.to_string(),
                tip.to_string(),
                sentinel.to_string(),
            ],
            |row| row.get(0),
        )?;
        Ok(!blocked)
    }

    /// Start of `parent`'s current keep-alive window: the later of the tip's
    /// idle start and the newest valve row this parent ever had.
    fn keepalive_window_start(&self, parent: Uuid, tip: Uuid) -> Result<Option<DateTime<Utc>>> {
        let idle = self.idle_since(tip)?;
        let last_valve: Option<String> = self.conn.query_row(
            "SELECT MAX(created_at) FROM scheduled_jobs WHERE name=?1",
            [keepalive_row_name(parent)],
            |row| row.get(0),
        )?;
        let last_valve =
            last_valve.and_then(|text| super::row_mappers::parse_timestamp(&text).ok());
        Ok(match (idle, last_valve) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        })
    }

    /// One reconcile pass: materialize at most one valve row per window for
    /// each eligible idle parent whose window has elapsed. Returns the ids of
    /// the rows inserted (an existing row for the window is left alone).
    pub(crate) fn reconcile_child_keepalives(
        &self,
        now: DateTime<Utc>,
        policy: &ChildAutonomyPolicy,
    ) -> Result<Vec<Uuid>> {
        if !policy.keepalive_enabled {
            return Ok(Vec::new());
        }
        let mut inserted = Vec::new();
        for parent in self.keepalive_candidates()? {
            let tip = self.published_lineage_tip(parent)?.unwrap_or(parent);
            if !self.keepalive_eligible(parent, tip)? {
                continue;
            }
            let running = self.running_children_of(parent)?;
            if running.is_empty() {
                continue;
            }
            let Some(window_start) = self.keepalive_window_start(parent, tip)? else {
                continue;
            };
            if now < window_start + policy.window {
                continue;
            }
            let id = keepalive_row_id(parent, window_start);
            if self.get_scheduled_job(&id)?.is_some() {
                continue;
            }
            let children = running
                .iter()
                .map(|child| child.tip.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let job = ScheduledJob {
                id,
                name: keepalive_row_name(parent),
                message: format!(
                    "Keep-alive: {} child session(s) are still running ({children}). Inspect \
                     their progress, nudge or halt any that are stuck, or keep waiting. This \
                     is a daemon keep-alive, not a completion signal.",
                    running.len()
                ),
                schedule: ScheduleSpec {
                    recurrence: Recurrence::Once,
                    anchor: now,
                },
                last_fired_at: None,
                next_fire_at: now,
                enabled: true,
                working_dir: None,
                provider: None,
                model: None,
                project_id: None,
                created_at: now,
                updated_at: now,
                wake_mode: WakeMode::Resume,
                wake_session_id: Some(parent),
            };
            self.insert_scheduled_job(&job)?;
            inserted.push(id);
        }
        Ok(inserted)
    }
}
