//! Scheduled-wake row construction and the program-guard sentinel identity,
//! moved down from `session::harness::tools::schedule_wake` so `store` has no
//! edge into `session` (issue #1021 S3a). The tool re-exports these at its old
//! path.

use chrono::{DateTime, TimeZone as _, Utc};
use rsi_common::schedule::{initial_next_fire_at, validate_creatable};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use std::path::PathBuf;
use std::sync::LazyLock;
use uuid::Uuid;

/// Already-parsed inputs for building a `ScheduledJob`. Shared between the
/// `schedule_wake` harness tool (agent-invoked, JSON-shaped args) and the
/// `AgentScheduleWake` RPC verb (session-attributed caller, typed params) so
/// the validation/construction logic in [`build_scheduled_job`] is not
/// duplicated (gate-pack invariant #7: one shared scheduling service). Each
/// caller owns its own argument extraction — the shapes differ slightly at
/// the boundary (JSON args vs. a `Deserialize` params struct) — and converges
/// here.
pub struct ScheduleWakeRequest {
    pub message: String,
    pub in_seconds: Option<i64>,
    pub at: Option<String>,
    pub name: Option<String>,
    pub every_seconds: Option<i64>,
    pub mode: Option<String>,
    pub working_dir: PathBuf,
    pub provider: Option<rsi_common::types::SessionProvider>,
    pub model: Option<String>,
    pub project_id: Option<Uuid>,
    /// Server-bound origin for agent-scheduled `resume`, `fresh`, and terminal
    /// watch jobs. `None` makes resume mode an error; an unbound generic Fresh
    /// job deliberately retains the default launch behavior.
    pub origin_session_id: Option<Uuid>,
    /// Watched-subject session for `mode:"on_terminal"` (A8 terminal watch).
    /// `None` makes `on_terminal` mode an error. Every arm transport
    /// authorizes this id (`AgentGetStatus` scope, self-watch rejected)
    /// BEFORE building the job: the RPC verb via
    /// `SessionManager::authorize_watch_target`, and — since A8.1 Q3 — the
    /// harness tool via its construction-time [`AgentControlHandle`].
    pub watch_session_id: Option<Uuid>,
}

use crate::store::scheduled_jobs::BACKGROUND_PROCESS_WAKE_PREFIX as DAEMON_PROCESS_WAKE_PREFIX;

/// Construction-time provenance for shared scheduled-wake row building.
///
/// Only token-resolved `AgentScheduleWake` and native tool construction with
/// both a bound origin and guarded agent control may select `AgentBound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScheduleWakeProvenance {
    Generic,
    AgentBound,
}

static PROGRAM_GUARD_FIRE_AT: LazyLock<DateTime<Utc>> = LazyLock::new(|| {
    Utc.with_ymd_and_hms(9999, 12, 31, 23, 59, 59)
        .single()
        .unwrap_or_else(|| panic!("year-9999 program guard timestamp must be representable"))
});

/// Deterministic daemon-owned identity for one session's program sentinel.
/// Agent-facing request shapes contain no row-id field, so only the
/// transport-bound caller can select this UUID.
pub fn deterministic_program_guard_job_id(session_id: Uuid) -> Uuid {
    let namespace = Uuid::from_u128(0xa7486a6b30b15c18af17d11eb99e9f8c);
    Uuid::new_v5(&namespace, session_id.to_string().as_bytes())
}

/// The essential immutable envelope that distinguishes a program guard from
/// an ordinary one-shot Resume wake. Enabled is deliberately not included:
/// typed terminal settlement disables the sentinel without erasing durable
/// program identity.
pub fn is_program_guard_sentinel(job: &ScheduledJob, session_id: Uuid) -> bool {
    job.id == deterministic_program_guard_job_id(session_id)
        && job.wake_mode == WakeMode::Resume
        && job.wake_session_id == Some(session_id)
        && matches!(job.schedule.recurrence, Recurrence::Once)
        && job.schedule.anchor == *PROGRAM_GUARD_FIRE_AT
        && job.next_fire_at == *PROGRAM_GUARD_FIRE_AT
}

/// Build and validate a `ScheduledJob` from a [`ScheduleWakeRequest`].
/// Returns `Err(message)` for the same validation failures the original
/// inline `ScheduleWakeTool::execute` body enforced (conflicting/missing
/// timing, non-positive intervals, a past one-shot `at`, invalid mode,
/// resume-without-origin) so every caller surfaces identical error text.
pub fn build_scheduled_job(req: ScheduleWakeRequest) -> Result<ScheduledJob, String> {
    build_scheduled_job_with_provenance(req, ScheduleWakeProvenance::Generic)
}

/// Daemon-only construction path for token-resolved `AgentScheduleWake` and
/// a native tool built with bound guarded authority. The provenance selector
/// is intentionally absent from the JSON-shaped request type.
pub fn build_agent_scheduled_job(req: ScheduleWakeRequest) -> Result<ScheduledJob, String> {
    build_scheduled_job_with_provenance(req, ScheduleWakeProvenance::AgentBound)
}

fn build_scheduled_job_with_provenance(
    req: ScheduleWakeRequest,
    provenance: ScheduleWakeProvenance,
) -> Result<ScheduledJob, String> {
    if req
        .name
        .as_deref()
        .is_some_and(|name| name.starts_with(DAEMON_PROCESS_WAKE_PREFIX))
    {
        return Err("wake names beginning with 'background-process-' are reserved".to_string());
    }
    let mode = req.mode.as_deref();
    if provenance == ScheduleWakeProvenance::AgentBound && mode.is_none() {
        return Err(
            "agent-scheduled wake requires explicit 'mode': choose 'fresh', 'resume', \
             'on_terminal', or 'program_guard'; use 'resume' to continue this session"
                .to_string(),
        );
    }
    if let Some(value) = mode {
        match value {
            "fresh" | "resume" | "on_terminal" | "program_guard" => {}
            _ => {
                return Err(format!(
                    "invalid wake mode '{value}': choose 'fresh', 'resume', 'on_terminal', or \
                     'program_guard'"
                ));
            }
        }
    }

    let is_watch = req.mode.as_deref() == Some("on_terminal");
    let is_program_guard = req.mode.as_deref() == Some("program_guard");
    if is_program_guard && provenance != ScheduleWakeProvenance::AgentBound {
        return Err("mode 'program_guard' requires daemon-bound agent authority".to_string());
    }

    let anchor: DateTime<Utc> = match (req.in_seconds, req.at.as_deref()) {
        (Some(_), _) | (_, Some(_)) if is_program_guard => {
            return Err("mode 'program_guard' uses daemon-controlled timing".to_string());
        }
        (Some(_), Some(_)) => {
            return Err("provide exactly one of 'in_seconds' or 'at', not both".to_string());
        }
        // A8: a terminal watch needs no explicit timing — anchor now; the
        // recurring row is the DB-reconcile tick (plan §3.2).
        (None, None) if is_program_guard => *PROGRAM_GUARD_FIRE_AT,
        (None, None) if is_watch => Utc::now(),
        (None, None) => return Err("provide exactly one of 'in_seconds' or 'at'".to_string()),
        (Some(secs), None) => {
            if secs <= 0 {
                return Err(format!("'in_seconds' must be positive, got {secs}"));
            }
            Utc::now() + chrono::Duration::seconds(secs)
        }
        (None, Some(s)) => match DateTime::parse_from_rfc3339(s) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(_) => return Err(format!("'at' must be a valid RFC3339 timestamp, got: {s}")),
        },
    };

    let recurrence = if is_program_guard && req.every_seconds.is_some() {
        return Err("mode 'program_guard' must be one-shot".to_string());
    } else if let Some(every) = req.every_seconds {
        if every <= 0 {
            return Err(format!("'every_seconds' must be positive, got {every}"));
        }
        Recurrence::EverySeconds(every as u64)
    } else if is_watch {
        // A8 D2/D3: a watch is a RECURRING row — every fire attempt re-reads
        // persisted session state, and requeue-until-idle rides the same
        // recurrence. One-shot watches would reintroduce the F-009 lost-wake.
        Recurrence::EverySeconds(60)
    } else {
        Recurrence::Once
    };

    let spec = ScheduleSpec { recurrence, anchor };

    let now = Utc::now();
    validate_creatable(&spec, now)?;

    let is_resume = matches!(req.mode.as_deref(), Some("resume" | "program_guard"));
    let is_agent_fresh =
        req.mode.as_deref() == Some("fresh") && provenance == ScheduleWakeProvenance::AgentBound;
    if is_resume && req.origin_session_id.is_none() {
        return Err(
            "mode 'resume' requires a known origin session id; this session has none".to_string(),
        );
    }
    if is_agent_fresh && req.origin_session_id.is_none() {
        return Err(
            "agent fresh construction requires a known origin session id; this session has none"
                .to_string(),
        );
    }
    let watch_target: Option<Uuid> = if is_watch {
        if req.origin_session_id.is_none() {
            return Err(
                "mode 'on_terminal' requires a known origin session id; this session has none"
                    .to_string(),
            );
        }
        match req.watch_session_id {
            Some(watched) => Some(watched),
            None => return Err("mode 'on_terminal' requires 'watch_session_id'".to_string()),
        }
    } else if req.watch_session_id.is_some() {
        return Err("'watch_session_id' is only valid with mode 'on_terminal'".to_string());
    } else {
        None
    };

    let name = if is_program_guard {
        format!(
            "master-orchestrate-program-guard-{}",
            req.origin_session_id
                .unwrap_or_else(|| panic!("program guard origin checked above"))
        )
    } else {
        req.name.unwrap_or_else(|| {
            if is_watch {
                "rsi-watch".to_string()
            } else {
                "agent-wake".to_string()
            }
        })
    };
    let next_fire_at = initial_next_fire_at(&spec, now);
    let now_ts = Utc::now();

    Ok(ScheduledJob {
        id: if is_program_guard {
            deterministic_program_guard_job_id(
                req.origin_session_id
                    .unwrap_or_else(|| panic!("program guard origin checked above")),
            )
        } else {
            Uuid::new_v4()
        },
        name,
        message: req.message,
        schedule: spec,
        last_fired_at: None,
        next_fire_at,
        enabled: true,
        working_dir: Some(req.working_dir),
        provider: req.provider,
        model: req.model,
        project_id: req.project_id,
        created_at: now_ts,
        updated_at: now_ts,
        wake_mode: watch_target.map_or(
            if is_resume {
                WakeMode::Resume
            } else if is_agent_fresh {
                WakeMode::AgentFresh
            } else {
                WakeMode::Fresh
            },
            WakeMode::OnTerminal,
        ),
        // Fresh must retain the same server-bound origin as Resume so the
        // scheduler can copy its rotation-disabled state before spawn. Generic
        // operator-created Fresh jobs pass `None` and remain unbound.
        wake_session_id: req.origin_session_id,
    })
}
