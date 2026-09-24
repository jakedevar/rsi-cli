//! schedule_wake harness tool — lets agents schedule future session launches.

use super::HarnessTool;
use crate::session::agent_verbs::{AgentControlHandle, ArmWatchOutcome};
use crate::session::harness::types::ToolResult;
use crate::store::Store;
use chrono::{DateTime, TimeZone as _, Utc};
use rsi_common::agent_control_schema::AgentControlVerbV1;
use rsi_common::schedule::{initial_next_fire_at, validate_creatable};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use tokio::sync::Mutex;
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
pub(crate) fn deterministic_program_guard_job_id(session_id: Uuid) -> Uuid {
    let namespace = Uuid::from_u128(0xa7486a6b30b15c18af17d11eb99e9f8c);
    Uuid::new_v5(&namespace, session_id.to_string().as_bytes())
}

/// The essential immutable envelope that distinguishes a program guard from
/// an ordinary one-shot Resume wake. Enabled is deliberately not included:
/// typed terminal settlement disables the sentinel without erasing durable
/// program identity.
pub(crate) fn is_program_guard_sentinel(job: &ScheduledJob, session_id: Uuid) -> bool {
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
pub(crate) fn build_agent_scheduled_job(req: ScheduleWakeRequest) -> Result<ScheduledJob, String> {
    build_scheduled_job_with_provenance(req, ScheduleWakeProvenance::AgentBound)
}

fn build_scheduled_job_with_provenance(
    req: ScheduleWakeRequest,
    provenance: ScheduleWakeProvenance,
) -> Result<ScheduledJob, String> {
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

/// Captured at construction from `LaunchConfig`; never exposed through the
/// JSON schema — the agent cannot widen its own session context.
pub struct ScheduleWakeTool {
    store: Arc<Mutex<Store>>,
    /// The RSI session UUID of the originating harness session.
    /// Used for `mode:"resume"` and `mode:"on_terminal"` jobs.
    origin_session_id: Option<Uuid>,
    /// Fallback working directory for launched sessions.
    default_working_dir: PathBuf,
    provider: Option<rsi_common::types::SessionProvider>,
    model: Option<String>,
    project_id: Option<Uuid>,
    /// A8.1 Q3: the same guarded authority the `Agent*` RPC verbs use. When
    /// present together with a bound `origin_session_id`, the tool advertises
    /// and accepts `mode:"on_terminal"` — Harness sessions arm terminal
    /// watches natively (their `shell` tool scrubs `RSI_*`, so the
    /// shell→`rsi-rpc` bridge cannot reach the tokened verb). Without it the
    /// pre-A8.1 schema and rejection behavior are preserved verbatim.
    agent_control: Option<AgentControlHandle>,
}

impl ScheduleWakeTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Mutex<Store>>,
        origin_session_id: Option<Uuid>,
        default_working_dir: PathBuf,
        provider: Option<rsi_common::types::SessionProvider>,
        model: Option<String>,
        project_id: Option<Uuid>,
        agent_control: Option<AgentControlHandle>,
    ) -> Self {
        Self {
            store,
            origin_session_id,
            default_working_dir,
            provider,
            model,
            project_id,
            agent_control,
        }
    }

    /// Watch arming needs BOTH the guarded authority handle and a bound
    /// origin session (the wake target). Only then is `mode:"on_terminal"`
    /// advertised or accepted.
    fn watch_capable(&self) -> bool {
        self.agent_control.is_some() && self.origin_session_id.is_some()
    }

    /// A8.1 Q3: native watch arming. Authorizes the watched subject through
    /// the SAME guarded handle as the `AgentScheduleWake` RPC verb, then
    /// routes the built row through the shared single-critical-section arm
    /// service (dedup + per-master cap + insert, identical error text). No
    /// scheduler nudge exists on this path — the scheduler handle lives on
    /// the RPC server — so an already-terminal subject fires on the next
    /// reconcile tick (the recurring 60s row) rather than instantly.
    async fn arm_terminal_watch(
        &self,
        args: &serde_json::Value,
        message: String,
        working_dir: PathBuf,
    ) -> ToolResult {
        let (Some(control), Some(origin)) = (self.agent_control.as_ref(), self.origin_session_id)
        else {
            return fail(
                "mode 'on_terminal' requires an agent-control authority handle to authorize \
                 'watch_session_id'; this session cannot arm terminal watches",
            );
        };
        let watched = match args.get("watch_session_id").and_then(|v| v.as_str()) {
            Some(raw) => match Uuid::parse_str(raw) {
                Ok(watched) => watched,
                Err(e) => return fail(format!("invalid watch_session_id: {e}")),
            },
            None => return fail("mode 'on_terminal' requires 'watch_session_id'"),
        };
        // Watched-subject scope = AgentGetStatus/AgentHalt scope
        // (self / direct child / child-of-led-Epic), self-watch rejected.
        if let Err(e) = control.authorize_watch_target(origin, watched).await {
            return fail(e.to_string());
        }

        let req = ScheduleWakeRequest {
            message,
            in_seconds: args.get("in_seconds").and_then(|v| v.as_i64()),
            at: args.get("at").and_then(|v| v.as_str()).map(String::from),
            name: args.get("name").and_then(|v| v.as_str()).map(String::from),
            every_seconds: args.get("every_seconds").and_then(|v| v.as_i64()),
            mode: Some("on_terminal".to_string()),
            working_dir,
            provider: self.provider,
            model: self.model.clone(),
            project_id: self.project_id,
            origin_session_id: Some(origin),
            watch_session_id: Some(watched),
        };
        let job = match build_agent_scheduled_job(req) {
            Ok(job) => job,
            Err(e) => return fail(e),
        };

        match control.arm_terminal_watch(origin, job).await {
            Ok(ArmWatchOutcome::Armed(job)) => ToolResult {
                success: true,
                output: format!(
                    "terminal watch '{}' (id={}) armed on session {watched}; fires within ~60s \
                     (scheduler reconcile tick) of the watched session going terminal",
                    job.name, job.id
                ),
                error_msg: None,
            },
            Ok(ArmWatchOutcome::Deduplicated(existing)) => ToolResult {
                success: true,
                output: format!(
                    "terminal watch on session {watched} already armed (job id={}); \
                     deduplicated — no new row created",
                    existing.id
                ),
                error_msg: None,
            },
            Err(e) => fail(e.to_string()),
        }
    }
}

fn fail(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error_msg: Some(msg.into()),
    }
}

/// Schema for a tool WITHOUT watch authority (no control handle and/or no
/// bound origin): the pre-A8.1 surface, verbatim — `mode` stays the two-value
/// enum and `watch_session_id` is not advertised.
const PARAMS_SCHEMA_BASE: &str = r#"{"type":"object","required":["message"],"properties":{
"message":{"type":"string","description":"Prompt to run when the wake fires"},
"in_seconds":{"type":"integer","description":"Fire this many seconds from now (mutually exclusive with at)"},
"at":{"type":"string","description":"RFC3339 absolute fire time (mutually exclusive with in_seconds)"},
"name":{"type":"string","description":"Optional human-readable job name"},
"every_seconds":{"type":"integer","description":"Optional: recurring interval in seconds; omit for one-shot"},
"mode":{"type":"string","enum":["fresh","resume"],"description":"'fresh' (default) is a consumed, best-effort root launch that transfers no hierarchy or lead authority; use AgentReserveSuccessor for master turnover. 'resume' re-invokes this session with context"}
}}"#;

#[async_trait::async_trait]
impl HarnessTool for ScheduleWakeTool {
    fn name(&self) -> &str {
        "schedule_wake"
    }

    fn description(&self) -> &str {
        if self.watch_capable() {
            "Schedule a future session wake-up. An explicit mode is required. In 'fresh' \
             mode performs a consumed, best-effort root launch after this one is terminal; \
             it transfers no hierarchy or lead authority, so use AgentReserveSuccessor for \
             master turnover. In 'resume' mode re-invokes this session \
             with its conversation context. In 'on_terminal' mode arms a daemon-owned \
             terminal watch that resumes this session when the watched session (a direct \
             child, or a child of an Epic this session leads) finishes; the watch fires \
             within ~60 seconds of the terminal transition. In 'program_guard' mode it \
             registers the deterministic master-orchestrate program sentinel. Jobs fire \
             within ~60 seconds of the requested time. One-shot or recurring."
        } else {
            "Schedule a future session wake-up. In 'fresh' mode (default) launches a \
             consumed, best-effort root session with the given message; it transfers no \
             hierarchy or lead authority, so use AgentReserveSuccessor for master turnover. \
             In 'resume' mode re-invokes this session \
             with its conversation context. Jobs fire within ~60 seconds of the requested \
             time. One-shot or recurring."
        }
    }

    fn parameters_json(&self) -> &str {
        if self.watch_capable() {
            AgentControlVerbV1::ScheduleWake
                .descriptor()
                .parameters_json()
        } else {
            PARAMS_SCHEMA_BASE
        }
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        let message = match args.get("message").and_then(|v| v.as_str()) {
            Some(m) => m.to_string(),
            None => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some("missing required argument: message".to_string()),
                };
            }
        };

        let effective_working_dir = if working_dir.as_os_str().is_empty() {
            self.default_working_dir.clone()
        } else {
            working_dir.to_path_buf()
        };

        let mode = args.get("mode").and_then(|v| v.as_str()).map(String::from);
        if mode.as_deref() == Some("on_terminal") {
            // A8.1 Q3: the watch path authorizes the watched subject through
            // the construction-time control handle and arms through the
            // shared dedup/cap/insert service. A handle-less tool still
            // rejects here (pre-A8.1 behavior, regression-pinned).
            return self
                .arm_terminal_watch(&args, message, effective_working_dir)
                .await;
        }
        if mode.as_deref() == Some("program_guard") {
            let (Some(control), Some(origin)) =
                (self.agent_control.as_ref(), self.origin_session_id)
            else {
                return fail(
                    "mode 'program_guard' requires an agent-control authority handle and bound origin",
                );
            };
            let job = match build_agent_scheduled_job(ScheduleWakeRequest {
                message,
                in_seconds: args.get("in_seconds").and_then(|value| value.as_i64()),
                at: args
                    .get("at")
                    .and_then(|value| value.as_str())
                    .map(String::from),
                name: args
                    .get("name")
                    .and_then(|value| value.as_str())
                    .map(String::from),
                every_seconds: args.get("every_seconds").and_then(|value| value.as_i64()),
                mode,
                working_dir: effective_working_dir,
                provider: self.provider,
                model: self.model.clone(),
                project_id: self.project_id,
                origin_session_id: Some(origin),
                watch_session_id: None,
            }) {
                Ok(job) => job,
                Err(error) => return fail(error),
            };
            return match control.register_program_guard(origin, job).await {
                Ok(crate::session::agent_verbs::ProgramGuardRegistration::Registered(job)) => {
                    ToolResult {
                        success: true,
                        output: format!("program guard registered (job id={})", job.id),
                        error_msg: None,
                    }
                }
                Ok(crate::session::agent_verbs::ProgramGuardRegistration::Deduplicated(job)) => {
                    ToolResult {
                        success: true,
                        output: format!(
                            "program guard already registered (job id={}); deduplicated",
                            job.id
                        ),
                        error_msg: None,
                    }
                }
                Err(error) => fail(error.to_string()),
            };
        }

        let req = ScheduleWakeRequest {
            message,
            in_seconds: args.get("in_seconds").and_then(|v| v.as_i64()),
            at: args.get("at").and_then(|v| v.as_str()).map(String::from),
            name: args.get("name").and_then(|v| v.as_str()).map(String::from),
            every_seconds: args.get("every_seconds").and_then(|v| v.as_i64()),
            mode,
            working_dir: effective_working_dir,
            provider: self.provider,
            model: self.model.clone(),
            project_id: self.project_id,
            origin_session_id: self.origin_session_id,
            // Non-watch modes never carry a watched subject; a smuggled
            // `watch_session_id` arg keeps its historical no-op here (the
            // RPC verb rejects it — arg-shape parity is on the watch path).
            watch_session_id: None,
        };

        let job = match if self.agent_control.is_some() && self.origin_session_id.is_some() {
            build_agent_scheduled_job(req)
        } else {
            build_scheduled_job(req)
        } {
            Ok(job) => job,
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(e),
                };
            }
        };

        let guard = self.store.lock().await;
        match guard.insert_scheduled_job(&job) {
            Ok(()) => ToolResult {
                success: true,
                output: format!(
                    "scheduled wake job '{}' (id={}) created; fires at {}",
                    job.name,
                    job.id,
                    job.next_fire_at.to_rfc3339()
                ),
                error_msg: None,
            },
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("failed to create scheduled job: {e}")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use std::path::Path;

    fn make_store() -> Arc<Mutex<Store>> {
        let store = Store::open_in_memory().expect("in-memory store");
        Arc::new(Mutex::new(store))
    }

    fn make_tool(store: Arc<Mutex<Store>>) -> ScheduleWakeTool {
        ScheduleWakeTool::new(
            store,
            Some(Uuid::new_v4()),
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None, // handle-less: not watch-capable
        )
    }

    /// Watch-capable fixture: a control handle over the SAME store as the
    /// tool (as in production, where both come from the `SessionManager`),
    /// with a persisted caller, its direct child, and an unrelated stranger.
    async fn make_watch_tool() -> (ScheduleWakeTool, Arc<Mutex<Store>>, Uuid, Uuid, Uuid) {
        use crate::session::agent_verbs::tests::test_session;
        use crate::session::spawn_coordinator::SpawnCoordinator;
        use std::collections::HashMap;

        let store = make_store();
        let caller = Uuid::new_v4();
        let child = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        {
            let guard = store.lock().await;
            guard
                .insert_session(&test_session(caller, PathBuf::from("/tmp")))
                .expect("insert caller");
            let mut child_row = test_session(child, PathBuf::from("/tmp"));
            child_row.parent_id = Some(caller);
            guard.insert_session(&child_row).expect("insert child");
            guard
                .insert_session(&test_session(stranger, PathBuf::from("/tmp")))
                .expect("insert stranger");
        }
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let (spawn_tx, _spawn_rx) = tokio::sync::mpsc::channel(4);
        let control = AgentControlHandle::new(
            active,
            completed,
            Arc::clone(&store),
            Arc::new(crate::bus::EventBus::new(16)),
            Arc::new(SpawnCoordinator::new(spawn_tx)),
        );
        let tool = ScheduleWakeTool::new(
            Arc::clone(&store),
            Some(caller),
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            Some(control),
        );
        (tool, store, caller, child, stranger)
    }

    #[tokio::test]
    async fn test_fresh_insert_in_seconds() {
        let store = make_store();
        let tool = make_tool(Arc::clone(&store));
        let args = serde_json::json!({
            "message": "hello",
            "in_seconds": 120
        });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(
            result.success,
            "expected success, got {:?}",
            result.error_msg
        );
        let guard = store.lock().await;
        let jobs = guard.list_scheduled_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].enabled);
        assert_eq!(jobs[0].wake_mode, rsi_common::types::WakeMode::Fresh);
    }

    #[tokio::test]
    async fn bound_native_explicit_fresh_persists_agent_fresh() {
        let (tool, store, caller, _child, _stranger) = make_watch_tool().await;
        let result = tool
            .execute(
                serde_json::json!({
                    "message": "next generation",
                    "in_seconds": 60,
                    "mode": "fresh"
                }),
                Path::new("/tmp"),
            )
            .await;
        assert!(result.success, "{:?}", result.error_msg);

        let jobs = store.lock().await.list_scheduled_jobs().expect("list jobs");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].wake_mode, WakeMode::AgentFresh);
        assert_eq!(jobs[0].wake_session_id, Some(caller));
    }

    #[tokio::test]
    async fn bound_native_omitted_or_unknown_mode_rejects_without_insert() {
        let (tool, store, _caller, _child, _stranger) = make_watch_tool().await;
        for args in [
            serde_json::json!({ "message": "ambiguous", "in_seconds": 60 }),
            serde_json::json!({
                "message": "ambiguous",
                "in_seconds": 60,
                "mode": "surprise"
            }),
        ] {
            let result = tool.execute(args, Path::new("/tmp")).await;
            assert!(!result.success);
            let error = result.error_msg.expect("typed construction error");
            assert!(
                error.contains("explicit 'mode'") || error.contains("invalid wake mode"),
                "unexpected error: {error}"
            );
        }
        assert!(
            store
                .lock()
                .await
                .list_scheduled_jobs()
                .expect("list jobs")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_past_once_at_rejects() {
        let store = make_store();
        let tool = make_tool(Arc::clone(&store));
        let args = serde_json::json!({
            "message": "hi",
            "at": "2020-01-01T00:00:00Z"
        });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(!result.success);
        assert!(result.error_msg.unwrap().contains("past"));
    }

    #[tokio::test]
    async fn test_missing_timing_rejects() {
        let store = make_store();
        let tool = make_tool(Arc::clone(&store));
        let args = serde_json::json!({ "message": "hi" });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_both_timing_rejects() {
        let store = make_store();
        let tool = make_tool(Arc::clone(&store));
        let args = serde_json::json!({
            "message": "hi",
            "in_seconds": 60,
            "at": "2030-01-01T00:00:00Z"
        });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(!result.success);
        assert!(result.error_msg.unwrap().contains("not both"));
    }

    #[tokio::test]
    async fn test_unknown_mode_rejects_without_insert() {
        let store = make_store();
        let tool = make_tool(Arc::clone(&store));
        let result = tool
            .execute(
                serde_json::json!({
                    "message": "ambiguous",
                    "in_seconds": 60,
                    "mode": "surprise"
                }),
                Path::new("/tmp"),
            )
            .await;
        assert!(!result.success);
        assert!(
            result
                .error_msg
                .expect("mode error")
                .contains("invalid wake mode")
        );
        assert!(
            store
                .lock()
                .await
                .list_scheduled_jobs()
                .expect("list jobs")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_non_positive_in_seconds_rejects() {
        let store = make_store();
        let tool = make_tool(Arc::clone(&store));
        let args = serde_json::json!({
            "message": "hi",
            "in_seconds": -5
        });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_non_positive_every_seconds_rejects() {
        let store = make_store();
        let tool = make_tool(Arc::clone(&store));
        let args = serde_json::json!({
            "message": "hi",
            "in_seconds": 60,
            "every_seconds": 0
        });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_resume_without_origin_rejects() {
        let store = make_store();
        let tool = ScheduleWakeTool::new(
            Arc::clone(&store),
            None, // no origin
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
        );
        let args = serde_json::json!({
            "message": "resume me",
            "in_seconds": 60,
            "mode": "resume"
        });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(!result.success);
        assert!(result.error_msg.unwrap().contains("origin"));
    }

    /// A8.1: a HANDLE-LESS tool (no agent-control authority) still cannot arm
    /// a watch — `mode:"on_terminal"` (even with a smuggled arg) is rejected
    /// and nothing is inserted. Watch capability requires the
    /// construction-time [`AgentControlHandle`], never tool args.
    #[tokio::test]
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::significant_drop_tightening
    )]
    async fn test_tool_on_terminal_mode_rejects() {
        let store = make_store();
        let tool = make_tool(Arc::clone(&store));
        let args = serde_json::json!({
            "message": "watch it",
            "mode": "on_terminal",
            "watch_session_id": Uuid::new_v4().to_string(),
        });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(!result.success);
        assert!(result.error_msg.unwrap().contains("watch_session_id"));
        let guard = store.lock().await;
        assert!(guard.list_scheduled_jobs().unwrap().is_empty());
    }

    /// A8 builder contract: `on_terminal` defaults (recurring 60s, name
    /// rsi-watch, wake target = origin) and its requirement matrix.
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn test_build_on_terminal_defaults_and_requirements() {
        let origin = Uuid::new_v4();
        let watched = Uuid::new_v4();
        let base = |origin_id: Option<Uuid>, watch_id: Option<Uuid>| ScheduleWakeRequest {
            message: "note".to_string(),
            in_seconds: None,
            at: None,
            name: None,
            every_seconds: None,
            mode: Some("on_terminal".to_string()),
            working_dir: PathBuf::from("/tmp"),
            provider: None,
            model: None,
            project_id: None,
            origin_session_id: origin_id,
            watch_session_id: watch_id,
        };

        let job = build_scheduled_job(base(Some(origin), Some(watched))).expect("build");
        assert_eq!(job.wake_mode, WakeMode::OnTerminal(watched));
        assert_eq!(job.wake_session_id, Some(origin));
        assert_eq!(job.name, "rsi-watch");
        assert_eq!(job.schedule.recurrence, Recurrence::EverySeconds(60));
        assert!(job.enabled);

        // Missing origin / missing watch id / watch id without the mode.
        assert!(build_scheduled_job(base(None, Some(watched))).is_err());
        assert!(build_scheduled_job(base(Some(origin), None)).is_err());
        let mut wrong_mode = base(Some(origin), Some(watched));
        wrong_mode.mode = None;
        wrong_mode.in_seconds = Some(60);
        assert!(build_scheduled_job(wrong_mode).is_err());
    }

    #[test]
    fn program_guard_builder_is_deterministic_daemon_bound_and_unsteerable() {
        let origin = Uuid::new_v4();
        let request = |in_seconds| ScheduleWakeRequest {
            message: "master-orchestrate program guard".into(),
            in_seconds,
            at: None,
            name: Some("caller-name-is-ignored".into()),
            every_seconds: None,
            mode: Some("program_guard".into()),
            working_dir: PathBuf::from("/sandbox/root"),
            provider: None,
            model: None,
            project_id: None,
            origin_session_id: Some(origin),
            watch_session_id: None,
        };

        assert!(build_scheduled_job(request(None)).is_err());
        assert!(build_agent_scheduled_job(request(Some(60))).is_err());
        let first = build_agent_scheduled_job(request(None)).expect("bound guard");
        let second = build_agent_scheduled_job(request(None)).expect("deterministic replay");
        assert_eq!(first.id, second.id);
        assert_eq!(first.id, deterministic_program_guard_job_id(origin));
        assert!(is_program_guard_sentinel(&first, origin));
        assert_eq!(first.wake_mode, WakeMode::Resume);
        assert_eq!(first.wake_session_id, Some(origin));
        assert!(first.name.starts_with("master-orchestrate-program-guard-"));
    }

    #[tokio::test]
    async fn test_resume_with_origin_inserts() {
        let store = make_store();
        let origin = Uuid::new_v4();
        let tool = ScheduleWakeTool::new(
            Arc::clone(&store),
            Some(origin),
            PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
        );
        let args = serde_json::json!({
            "message": "resume me",
            "in_seconds": 60,
            "mode": "resume"
        });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(
            result.success,
            "expected success, got {:?}",
            result.error_msg
        );
        let guard = store.lock().await;
        let jobs = guard.list_scheduled_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].wake_mode, rsi_common::types::WakeMode::Resume);
        assert_eq!(jobs[0].wake_session_id, Some(origin));
    }

    /// A8.1 Q3: the advertised schema follows watch capability — a
    /// handle-ful tool advertises `on_terminal` + `watch_session_id`; a
    /// handle-less tool keeps the pre-A8.1 schema verbatim.
    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn test_schema_advertises_watch_only_when_capable() {
        let (watch_tool, _store, _caller, _child, _stranger) = make_watch_tool().await;
        let schema: serde_json::Value =
            serde_json::from_str(watch_tool.parameters_json()).expect("valid schema json");
        let modes = schema["properties"]["mode"]["enum"]
            .as_array()
            .expect("mode enum");
        assert!(modes.contains(&serde_json::json!("on_terminal")));
        assert!(modes.contains(&serde_json::json!("program_guard")));
        assert!(schema["properties"]["watch_session_id"].is_object());
        assert!(
            schema["required"]
                .as_array()
                .expect("required array")
                .contains(&serde_json::json!("mode"))
        );
        assert!(watch_tool.description().contains("on_terminal"));
        assert!(
            watch_tool
                .description()
                .contains("explicit mode is required")
        );
        assert!(
            watch_tool
                .description()
                .contains("transfers no hierarchy or lead authority")
        );
        assert!(watch_tool.description().contains("AgentReserveSuccessor"));
        assert!(
            schema["properties"]["mode"]["description"]
                .as_str()
                .unwrap()
                .contains("AgentReserveSuccessor")
        );

        let plain_tool = make_tool(make_store());
        assert_eq!(plain_tool.parameters_json(), PARAMS_SCHEMA_BASE);
        let plain_schema: serde_json::Value =
            serde_json::from_str(plain_tool.parameters_json()).expect("valid base schema");
        assert!(
            !plain_schema["required"]
                .as_array()
                .expect("required array")
                .contains(&serde_json::json!("mode")),
            "generic handle-less scheduling retains its Fresh default"
        );
        assert!(!plain_tool.parameters_json().contains("on_terminal"));
        assert!(!plain_tool.parameters_json().contains("program_guard"));
        assert!(!plain_tool.description().contains("on_terminal"));
        assert!(plain_tool.description().contains("AgentReserveSuccessor"));
        assert!(
            plain_schema["properties"]["mode"]["description"]
                .as_str()
                .unwrap()
                .contains("transfers no hierarchy or lead authority")
        );
    }

    /// A8.1 Q3: a handle-ful tool arms a watch on an authorized subject (the
    /// caller's direct child) through the shared arm service — the persisted
    /// row carries `OnTerminal(child)` with the wake target bound to the
    /// caller, exactly like the RPC verb.
    #[tokio::test]
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::significant_drop_tightening
    )]
    async fn test_tool_arms_watch_on_direct_child() {
        let (tool, store, caller, child, _stranger) = make_watch_tool().await;
        let args = serde_json::json!({
            "message": "watch it",
            "mode": "on_terminal",
            "watch_session_id": child.to_string(),
        });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(
            result.success,
            "expected success, got {:?}",
            result.error_msg
        );
        assert!(result.output.contains("armed"), "{}", result.output);

        let guard = store.lock().await;
        let jobs = guard.list_scheduled_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].wake_mode, WakeMode::OnTerminal(child));
        assert_eq!(jobs[0].wake_session_id, Some(caller));
        assert!(jobs[0].enabled);
    }

    /// A8.1 Q3: an out-of-scope watched subject is denied by the SAME guarded
    /// authority the RPC verb uses, and nothing is inserted.
    #[tokio::test]
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::significant_drop_tightening
    )]
    async fn test_tool_watch_unauthorized_target_rejects() {
        let (tool, store, _caller, _child, stranger) = make_watch_tool().await;
        let args = serde_json::json!({
            "message": "watch it",
            "mode": "on_terminal",
            "watch_session_id": stranger.to_string(),
        });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(!result.success);
        assert!(
            result
                .error_msg
                .unwrap()
                .contains("agent_verb_scope_denied")
        );
        let guard = store.lock().await;
        assert!(guard.list_scheduled_jobs().unwrap().is_empty());
    }

    /// A8.1 Q3: self-watch is rejected explicitly (same as the RPC verb).
    #[tokio::test]
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::significant_drop_tightening
    )]
    async fn test_tool_watch_self_rejects() {
        let (tool, store, caller, _child, _stranger) = make_watch_tool().await;
        let args = serde_json::json!({
            "message": "watch me",
            "mode": "on_terminal",
            "watch_session_id": caller.to_string(),
        });
        let result = tool.execute(args, Path::new("/tmp")).await;
        assert!(!result.success);
        assert!(result.error_msg.unwrap().contains("watch_self_rejected"));
        let guard = store.lock().await;
        assert!(guard.list_scheduled_jobs().unwrap().is_empty());
    }

    /// A8.1 Q3: a second identical arm deduplicates through the shared
    /// service — the tool says so, and no second row is created.
    #[tokio::test]
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::significant_drop_tightening
    )]
    async fn test_tool_second_identical_arm_deduplicates() {
        let (tool, store, _caller, child, _stranger) = make_watch_tool().await;
        let args = serde_json::json!({
            "message": "watch it",
            "mode": "on_terminal",
            "watch_session_id": child.to_string(),
        });
        let first = tool.execute(args.clone(), Path::new("/tmp")).await;
        assert!(first.success, "{:?}", first.error_msg);
        let second = tool.execute(args, Path::new("/tmp")).await;
        assert!(second.success, "{:?}", second.error_msg);
        assert!(second.output.contains("deduplicated"), "{}", second.output);
        let guard = store.lock().await;
        assert_eq!(guard.list_scheduled_jobs().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn native_program_guard_registration_is_deterministic_and_idempotent() {
        let (tool, store, caller, _child, _stranger) = make_watch_tool().await;
        let args = serde_json::json!({
            "message": "master-orchestrate program guard",
            "mode": "program_guard",
        });
        let first = tool.execute(args.clone(), Path::new("/sandbox/root")).await;
        assert!(first.success, "{:?}", first.error_msg);
        let second = tool.execute(args, Path::new("/sandbox/root")).await;
        assert!(second.success, "{:?}", second.error_msg);
        assert!(second.output.contains("deduplicated"));

        let guard = store.lock().await;
        let jobs = guard.list_scheduled_jobs().expect("list jobs");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, deterministic_program_guard_job_id(caller));
        assert!(is_program_guard_sentinel(&jobs[0], caller));
    }
}
