//! UntilCondition evaluation state machine for the loop executor (P1.11).
//!
//! # Role
//!
//! `UntilEvaluator` sits between the loop driver in `graph_runner.rs` and the
//! daemon's EventBus. After each iteration of a loop region completes, the
//! driver calls `check_async()` to decide whether to continue or halt.
//!
//! # Variants
//!
//! - `MaxIterations(n)`: halt after `n` iterations (sync check).
//! - `LeadHalt`: subscribe to `EventBus` for `DaemonEvent::HaltDirective { directive: "/halt" }`;
//!   set `halt_flag` atomically when detected (async subscriber task).
//! - `Predicate(String)`: V1 stub — only `"index_exhausted"` is evaluated
//!   (async filesystem check); all other expressions log warn + halt with `NoUntilGuard`.
//!
//! # Precedence (D5 from plan)
//!
//! `LeadHalt > MaxIterations > Predicate > NoUntilGuard`
//!
//! If multiple halt reasons fire on the same iteration, the highest-precedence
//! reason is reported and all reasons are logged at debug level.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use rsi_common::types::{IndexStatusValue, Role, UntilCondition};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::bus::{DaemonEvent, EventBus};
use crate::session::SessionManager;
use rsi_common::rpc::GetIndexStatusParams;

// ─── Public types ─────────────────────────────────────────────────────────────

/// Signal returned by `UntilEvaluator::check` / `check_async` after each
/// iteration to tell the loop driver whether to continue or halt.
#[derive(Debug)]
pub(crate) enum UntilSignal {
    Continue,
    Halt(HaltReason),
}

/// The reason the loop halted. Multiple reasons may fire on the same
/// iteration; the caller logs all and reports the highest-precedence one.
#[derive(Debug, Clone)]
pub(crate) enum HaltReason {
    /// Lead session emitted `<docregblock>/halt</docregblock>`.
    LeadHalt { source_session: Uuid },
    /// `MaxIterations(n)` condition reached.
    MaxIterationsReached(u32),
    /// `Predicate("index_exhausted")` — no active tickets remain.
    PredicateMet(String),
    /// `UntilCondition` absent or predicate not yet implemented in V1.
    NoUntilGuard,
}

/// Numeric priority for halt reasons (lower = higher precedence per D5).
fn halt_reason_priority(r: &HaltReason) -> u8 {
    match r {
        HaltReason::LeadHalt { .. } => 0,
        HaltReason::MaxIterationsReached(_) => 1,
        HaltReason::PredicateMet(_) => 2,
        HaltReason::NoUntilGuard => 3,
    }
}

// ─── UntilEvaluator ──────────────────────────────────────────────────────────

/// State machine that evaluates one `UntilCondition` over the lifetime of
/// a loop region execution.
pub(crate) struct UntilEvaluator {
    condition: UntilCondition,
    /// Number of iterations completed so far (incremented at the start of each check).
    pub(crate) iteration: u32,
    /// Set to `true` by the LeadHalt subscriber task when a `/halt` directive is detected.
    halt_flag: Arc<AtomicBool>,
    /// UUID of the session that emitted the `/halt` directive.
    halt_source: Arc<Mutex<Option<Uuid>>>,
    /// Subscriber task handle; dropped when the evaluator is dropped (cancels the task).
    _subscriber_handle: Option<JoinHandle<()>>,
}

impl UntilEvaluator {
    /// Construct a new evaluator for the given condition.
    ///
    /// For `LeadHalt`, spawns a background tokio task that subscribes to the
    /// EventBus and sets `halt_flag` when a matching `HaltDirective` arrives.
    /// For all other variants, no background task is spawned.
    ///
    /// `event_bus` and `session_manager` are taken by value and cloned into
    /// the spawned subscriber task below (constructor/ownership-transfer
    /// pattern) — `clippy::needless_pass_by_value` doesn't see that use as
    /// "consuming" the value, hence the allow.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn new(
        condition: UntilCondition,
        event_bus: Arc<EventBus>,
        session_manager: Arc<SessionManager>,
    ) -> Self {
        let halt_flag = Arc::new(AtomicBool::new(false));
        let halt_source: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));

        let subscriber_handle = if matches!(condition, UntilCondition::LeadHalt) {
            let flag = Arc::clone(&halt_flag);
            let source = Arc::clone(&halt_source);
            let mut rx = event_bus.subscribe();
            let eb = Arc::clone(&event_bus);
            let sm = Arc::clone(&session_manager);

            let handle = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            if let DaemonEvent::HaltDirective {
                                session_id,
                                ref directive,
                            } = *event
                            {
                                if directive == "/halt" {
                                    tracing::info!(
                                        session_id = %session_id,
                                        "UntilEvaluator: received /halt directive — setting halt_flag"
                                    );
                                    if let Ok(mut guard) = source.lock() {
                                        *guard = Some(session_id);
                                    }
                                    flag.store(true, Ordering::Relaxed);
                                    eb.unsubscribe();
                                    return;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            eb.unsubscribe();
                            return;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                lagged = n,
                                "UntilEvaluator LeadHalt subscriber lagged — re-checking active \
                                 sessions directly for a missed /halt directive"
                            );
                            // `/halt` is typically emitted only once, in the final chunk of
                            // a turn — a lagged receiver at that exact instant means the
                            // live `HaltDirective` publish is gone for good (broadcast does
                            // not redeliver skipped messages). Re-derive the same signal
                            // directly from each active session's current-turn assistant
                            // content instead of waiting on the bus.
                            if let Some(session_id) = find_missed_halt_directive(&sm).await {
                                tracing::info!(
                                    session_id = %session_id,
                                    "UntilEvaluator: recovered /halt directive after bus lag — setting halt_flag"
                                );
                                if let Ok(mut guard) = source.lock() {
                                    *guard = Some(session_id);
                                }
                                flag.store(true, Ordering::Relaxed);
                                eb.unsubscribe();
                                return;
                            }
                        }
                    }
                }
            });

            Some(handle)
        } else {
            None
        };

        Self {
            condition,
            iteration: 0,
            halt_flag,
            halt_source,
            _subscriber_handle: subscriber_handle,
        }
    }

    /// Synchronous check. Increments the iteration counter and evaluates all
    /// non-async halt sources. Returns `Continue` or `Halt(reason)`.
    ///
    /// For `Predicate` conditions, use `check_async` which can perform I/O.
    pub(crate) fn check(&mut self) -> UntilSignal {
        self.iteration += 1;

        let mut fired: Vec<HaltReason> = Vec::new();

        // LeadHalt check (highest priority, always checked regardless of condition variant).
        if self.halt_flag.load(Ordering::Relaxed) {
            let source = self
                .halt_source
                .lock()
                .ok()
                .and_then(|g| *g)
                .unwrap_or(Uuid::nil());
            fired.push(HaltReason::LeadHalt {
                source_session: source,
            });
        }

        match &self.condition {
            UntilCondition::MaxIterations(n) => {
                if self.iteration >= *n {
                    fired.push(HaltReason::MaxIterationsReached(*n));
                }
            }
            UntilCondition::LeadHalt => {
                // Handled above via halt_flag; no additional action needed.
            }
            UntilCondition::Predicate(_) => {
                // Predicate check requires async I/O — caller must use check_async().
                // If check() is called for a Predicate condition, treat as NoUntilGuard.
                fired.push(HaltReason::NoUntilGuard);
            }
        }

        resolve_signal(fired, self.iteration)
    }

    /// Async check. Equivalent to `check()` but also evaluates `Predicate` conditions
    /// that require I/O (filesystem INDEX.status.json reads).
    pub(crate) async fn check_async(
        &mut self,
        session_manager: &Arc<SessionManager>,
        project_id: Option<Uuid>,
    ) -> UntilSignal {
        self.iteration += 1;

        let mut fired: Vec<HaltReason> = Vec::new();

        // LeadHalt check (always, highest priority).
        if self.halt_flag.load(Ordering::Relaxed) {
            let source = self
                .halt_source
                .lock()
                .ok()
                .and_then(|g| *g)
                .unwrap_or(Uuid::nil());
            fired.push(HaltReason::LeadHalt {
                source_session: source,
            });
        }

        match &self.condition {
            UntilCondition::MaxIterations(n) => {
                if self.iteration >= *n {
                    fired.push(HaltReason::MaxIterationsReached(*n));
                }
            }
            UntilCondition::LeadHalt => {
                // Handled above via halt_flag.
            }
            UntilCondition::Predicate(expr) => {
                let expr = expr.clone();
                if expr == "index_exhausted" {
                    if check_predicate_index_exhausted(session_manager, project_id).await {
                        fired.push(HaltReason::PredicateMet("index_exhausted".to_string()));
                    }
                    // If not exhausted, no halt reason added → Continue.
                } else {
                    tracing::warn!(
                        predicate = %expr,
                        "UntilEvaluator: predicate '{}' not implemented in V1 — halting with NoUntilGuard",
                        expr
                    );
                    fired.push(HaltReason::NoUntilGuard);
                }
            }
        }

        resolve_signal(fired, self.iteration)
    }
}

impl Drop for UntilEvaluator {
    fn drop(&mut self) {
        if let Some(handle) = self._subscriber_handle.take() {
            handle.abort();
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Resolve a list of fired halt reasons into a `UntilSignal`.
///
/// If none fired → `Continue`.
/// If one or more fired → `Halt(highest_priority_reason)` with debug-level
/// logging when multiple fired simultaneously.
fn resolve_signal(fired: Vec<HaltReason>, iteration: u32) -> UntilSignal {
    if fired.is_empty() {
        return UntilSignal::Continue;
    }

    if fired.len() > 1 {
        tracing::debug!(
            iteration,
            "P1.11 UntilEvaluator: multiple halt reasons fired: {:?}",
            fired.iter().map(|r| format!("{:?}", r)).collect::<Vec<_>>()
        );
    }

    let best = fired
        .into_iter()
        .min_by_key(halt_reason_priority)
        .expect("fired is non-empty");

    UntilSignal::Halt(best)
}

/// Lag catch-up for `UntilCondition::LeadHalt`: re-scan every active
/// session's current-turn assistant content for the `/halt` directive.
///
/// Mirrors the turn-boundary + regex logic the live monitor path uses to
/// decide when to publish `DaemonEvent::HaltDirective` (see
/// `monitor.rs`'s `accumulated_assistant_content`, which clears on each
/// `Role::User` event and accumulates `Role::Assistant` content until the
/// next one): `TrackedSession::events` is the same live, appended-to event
/// list, so replaying it here reconstructs the identical "current turn"
/// window without needing the bus event that was dropped.
async fn find_missed_halt_directive(session_manager: &Arc<SessionManager>) -> Option<Uuid> {
    let active_sessions = session_manager.active();
    // Scoped so the read guard is dropped as soon as the scan is done,
    // rather than lingering until this function returns.
    let active = active_sessions.read().await;
    active.iter().find_map(|(session_id, tracked)| {
        let mut turn_content = String::new();
        for event in &tracked.events {
            match event.role {
                Some(Role::User) => turn_content.clear(),
                Some(Role::Assistant) => turn_content.push_str(&event.content),
                _ => {}
            }
        }
        super::types::HALT_DIRECTIVE_RE
            .is_match(&turn_content)
            .then_some(*session_id)
    })
}

/// V1 predicate implementation for `"index_exhausted"`.
///
/// Returns `true` (exhausted) if no tickets have status in
/// `{Ready, NotStarted, InProgress}`. Returns `false` (not exhausted) on any
/// error (no project, missing sidecar, parse failure) — conservative: never
/// halt the loop due to an I/O error.
pub(crate) async fn check_predicate_index_exhausted(
    session_manager: &Arc<SessionManager>,
    project_id: Option<Uuid>,
) -> bool {
    // Look up the project name from the project_id (needed for get_index_status).
    let project_name = match project_id {
        Some(pid) => match session_manager.get_project(pid).await {
            Ok(Some(project)) => project.name,
            Ok(None) => {
                tracing::warn!(
                    project_id = %pid,
                    "UntilEvaluator index_exhausted: project not found — treating as not exhausted"
                );
                return false;
            }
            Err(e) => {
                tracing::warn!(
                    project_id = %pid,
                    error = %e,
                    "UntilEvaluator index_exhausted: failed to look up project — treating as not exhausted"
                );
                return false;
            }
        },
        None => {
            tracing::warn!(
                "UntilEvaluator index_exhausted: no project_id — treating as not exhausted"
            );
            return false;
        }
    };

    let sidecar = match session_manager.get_index_status(GetIndexStatusParams {
        project: project_name.clone(),
    }) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                project = %project_name,
                error = %e,
                "UntilEvaluator index_exhausted: failed to read INDEX.status.json — treating as not exhausted"
            );
            return false;
        }
    };

    // Exhausted if no ticket has an "active" status.
    let active_statuses = [
        IndexStatusValue::Ready,
        IndexStatusValue::NotStarted,
        IndexStatusValue::InProgress,
    ];

    let has_active = sidecar
        .tickets
        .values()
        .any(|t| active_statuses.contains(&t.status));

    !has_active
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RuntimeConfig};
    use crate::session::types::TrackedSession;
    use crate::store::Store;
    use rsi_common::types::{
        ContextUsageConfidence, ConversationEvent, EventType, Session, SessionKind,
        SessionProvider, SessionStatus,
    };
    use tempfile::TempDir;

    /// Minimal `SessionManager` for tests — mirrors `session::tests::manager()`.
    fn manager() -> (SessionManager, TempDir) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("rsi.db");
        let store = Store::open(&db_path).expect("open store");
        let config = Config::from_env();
        let runtime_config = RuntimeConfig::from_config(&config);
        let manager = SessionManager::new(
            Arc::new(EventBus::new(16)),
            store,
            false,
            dir.path().join("daemon.sock"),
            None,
            Vec::new(),
            runtime_config,
            dir.path().join("sandboxes"),
        )
        .expect("manager");
        (manager, dir)
    }

    /// Minimal `Session` for tests — mirrors `session::tests::bare_session()`.
    fn bare_session(id: Uuid) -> Session {
        let now = chrono::Utc::now();
        Session {
            context_fill_pct: None,
            id,
            status: SessionStatus::Running,
            session_kind: SessionKind::Standard,
            provider: SessionProvider::default(),
            context_usage_confidence: ContextUsageConfidence::default(),
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            created_at: now,
            updated_at: now,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            working_dir: std::path::PathBuf::from("/tmp"),
            git_branch: None,
            model: None,
            claude_session_id: None,
            project_id: None,
            continued_from: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            stop_reason: None,
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            workflow_id: None,
            workflow_id_override: None,
            pipeline_artifact: None,
            pending_question: None,
            pending_archive: false,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: Some(0),
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    fn assistant_event(session_id: Uuid, sequence: i32, content: &str) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id,
            sequence,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            content: content.to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    fn user_event(session_id: Uuid, sequence: i32, content: &str) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id,
            sequence,
            event_type: EventType::Message,
            role: Some(Role::User),
            content: content.to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    /// Regression test for the broadcast-bus-lag silent-drop defect
    /// (`fix/bus-lag-silent-drop`, part (b)): the `LeadHalt` subscriber used
    /// to log-and-continue on `RecvError::Lagged` with no catch-up. Since
    /// `/halt` is emitted only once, in the final chunk of a turn, a lagged
    /// receiver at that exact instant means the live `HaltDirective` publish
    /// is gone for good (broadcast does not redeliver skipped messages) and
    /// `UntilCondition::LeadHalt` would never fire.
    ///
    /// This exercises `find_missed_halt_directive` directly (the catch-up
    /// the `Lagged` arm now calls) against an active session whose current
    /// turn already contains the `/halt` directive, and asserts it recovers
    /// the session id that the missed bus event would have carried.
    #[tokio::test]
    async fn find_missed_halt_directive_recovers_current_turn_halt() {
        let (manager, _dir) = manager();
        let manager = Arc::new(manager);

        let session_id = Uuid::new_v4();
        let mut tracked = TrackedSession::new_for_test(bare_session(session_id));
        tracked.events = vec![
            user_event(session_id, 1, "please loop until done"),
            assistant_event(session_id, 2, "working on it...\n"),
            assistant_event(session_id, 3, "<docregblock>\n/halt\n</docregblock>"),
        ];
        manager.active().write().await.insert(session_id, tracked);

        let recovered = find_missed_halt_directive(&manager).await;
        assert_eq!(
            recovered,
            Some(session_id),
            "lag catch-up must recover a /halt directive present in the current turn"
        );
    }

    /// Turn-boundary correctness: a `/halt` directive left over from a
    /// *prior* turn (before the most recent `Role::User` event) must NOT
    /// false-positive the catch-up scan — mirrors the live monitor path,
    /// which clears `accumulated_assistant_content` on every `Role::User`
    /// event.
    #[tokio::test]
    async fn find_missed_halt_directive_ignores_stale_prior_turn_halt() {
        let (manager, _dir) = manager();
        let manager = Arc::new(manager);

        let session_id = Uuid::new_v4();
        let mut tracked = TrackedSession::new_for_test(bare_session(session_id));
        tracked.events = vec![
            assistant_event(session_id, 1, "<docregblock>\n/halt\n</docregblock>"),
            user_event(session_id, 2, "actually keep going"),
            assistant_event(session_id, 3, "sure, continuing"),
        ];
        manager.active().write().await.insert(session_id, tracked);

        let recovered = find_missed_halt_directive(&manager).await;
        assert_eq!(
            recovered, None,
            "a /halt directive from a stale prior turn must not fire the catch-up"
        );
    }
}
