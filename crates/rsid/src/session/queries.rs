//! Read-only query methods: get/list sessions, conversations, turn metrics, health status.

use crate::error::{DaemonError, Result};
use crate::profiling;
use crate::sandbox::{SandboxAllocation, SandboxAllocator};
use rsi_common::types::{
    ConversationEvent, SandboxCleanupState, SandboxKind, Session, SessionDiagnosticV1, TurnMetric,
};
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::RwLock;
use uuid::Uuid;

use super::{CompletedSession, SessionManager, TrackedSession};

/// Reattach catalog and official descriptive facts to a persisted capability
/// tuple before it crosses an RPC boundary. V99 stores the active scalar and
/// provenance, not the full provider envelope; the exact version+digest fence
/// in the canonical registry decides whether catalog enrichment is valid.
fn rehydrate_context_budget_projection(session: &mut Session) {
    let Some(resolved) = session.resolved_context_budget.clone() else {
        return;
    };
    session.resolved_context_budget = Some(
        crate::provider_capabilities::rehydrate_resolved_context_budget(
            session.provider,
            session.model.as_deref().unwrap_or("unknown"),
            resolved,
        ),
    );
}

fn fresh_unarchive_sandbox_binding(
    session: &Session,
    sandbox_base: std::path::PathBuf,
) -> Result<(
    SandboxAllocation,
    crate::store::sandbox_custody::SessionCustodyBinding,
)> {
    let working_dir = session.working_dir.canonicalize().map_err(|error| {
        DaemonError::InvalidParam(format!(
            "cannot recreate sandbox: working directory '{}' is unavailable: {error}",
            session.working_dir.display()
        ))
    })?;
    let source_commit = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .current_dir(&working_dir)
        .output()
        .map_err(|error| {
            DaemonError::Process(format!(
                "failed to resolve fresh sandbox source commit: {error}"
            ))
        })?;
    if !source_commit.status.success() {
        return Err(DaemonError::InvalidParam(
            "cannot recreate sandbox: source commit is unavailable".to_string(),
        ));
    }
    let source_commit = String::from_utf8_lossy(&source_commit.stdout)
        .trim()
        .to_string();
    let selection = crate::sandbox::git_worktree::fresh_rolling_base(
        &working_dir,
        Uuid::new_v4(),
        source_commit,
        crate::sandbox::git_worktree::RollingBasePolicy::RemoteTip,
    )?;
    let source_commit = selection.commit.clone();
    let allocation = selection.allocate_with_cleanup(|commit| {
        SandboxAllocator::new(sandbox_base.clone()).allocate_replacement(
            session.id,
            &working_dir,
            SandboxKind::GitWorktree,
            commit,
            None,
        )
    })?;
    let binding = super::launch::new_root_binding_from_allocation(
        &allocation,
        &working_dir,
        Some(&source_commit),
        crate::store::sandbox_custody::CustodyCause::FreshLaunch,
        None,
    )?;
    Ok((allocation, binding))
}

/// Resolve a session snapshot from the in-memory `active`/`completed` maps
/// with a store fallback, stamping the derived-on-read `context_fill_pct`.
///
/// Extracted from [`SessionManager::get_session`] so collaborators that hold
/// only the underlying `Arc`s (rather than a `&SessionManager`) — notably the
/// P2 [`super::agent_verbs::AgentControlHandle`] used by the native
/// `rsi_control` tools — resolve sessions through the exact same path the
/// RPC verbs use. Single source of truth for read semantics.
pub(super) async fn get_session_snapshot(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    session_id: Uuid,
) -> Option<Session> {
    if let Some(tracked) = active.read().await.get(&session_id) {
        let mut s = tracked.session.clone();
        project_approval_started_at(&mut s, tracked);
        rehydrate_context_budget_projection(&mut s);
        s.context_fill_pct = super::monitor::context_fill_pct_for_tracked(tracked);
        return Some(s);
    }
    if let Some(completed) = completed.read().await.get(&session_id) {
        let mut s = completed.session.clone();
        rehydrate_context_budget_projection(&mut s);
        s.context_fill_pct = super::monitor::context_fill_pct_from_persisted(&s);
        return Some(s);
    }
    let store = store.clone();
    match tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.get_session(session_id)
    })
    .await
    {
        Ok(Ok(session)) => session.map(|mut s| {
            rehydrate_context_budget_projection(&mut s);
            s.context_fill_pct = super::monitor::context_fill_pct_from_persisted(&s);
            s
        }),
        Ok(Err(e)) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "get_session store fallback failed"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "get_session store fallback task failed"
            );
            None
        }
    }
}

/// Project the in-memory `TrackedSession.approval_wait_start: Option<Instant>`
/// into a wall-clock `DateTime<Utc>` and write it into the cloned `Session`.
///
/// `Instant` is monotonic with no calendar anchor, so we derive
/// `Utc::now() - elapsed()` per call. Sub-second drift is invisible at the
/// 1Hz repaint cadence the TUI uses for the live counter.
///
/// Lives in this module (`crates/rsid/src/session/queries.rs`) — sibling to
/// `types.rs` where `approval_wait_start` is declared `pub(super)`. This
/// visibility constraint is the load-bearing reason the projection is here
/// rather than in `crates/rsid/src/rpc.rs`.
fn project_approval_started_at(session: &mut Session, tracked: &TrackedSession) {
    session.approval_started_at = tracked.approval_wait_start.map(|started| {
        let elapsed = chrono::Duration::from_std(started.elapsed()).unwrap_or_default();
        chrono::Utc::now() - elapsed
    });
}

fn is_visible_in_session_list(session: &Session) -> bool {
    !matches!(
        session.status,
        rsi_common::types::SessionStatus::Archived | rsi_common::types::SessionStatus::Deleted
    )
}

/// Load a completed session's transcript from SQLite.
///
/// C7 Phase 1: `SessionManager::restore_sessions` inserts every restored
/// Completed/Failed/Interrupted session with `events: Vec::new()` and
/// `events_hydrated: false` instead of eagerly loading the full transcript,
/// so the daemon does not pull an operator's entire session history (order of
/// a GB, in the measured case) into RAM at startup. This is the shared
/// on-demand loader callers use to hydrate the real transcript the first time
/// it's actually needed (a TUI poll, a resume, a rotation). `load_events` is
/// index-backed (`idx_events_session_sequence`), so this is a cheap query.
pub(super) async fn load_completed_events_from_store(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    session_id: Uuid,
) -> Result<Vec<ConversationEvent>> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.load_events(session_id)
    })
    .await
    .map_err(|e| DaemonError::Store(e.to_string()))?
}

impl SessionManager {
    /// Stamp the derived-on-read `context_fill_pct` onto each session before it
    /// is returned over RPC. The daemon is the single producer of this value:
    /// active sessions use their live runtime state (matching the
    /// `ContextUsageUpdated` bus event), idle/persisted sessions are computed
    /// from stored token fields — both through the one shared formula in
    /// `monitor.rs`. Never persisted; recomputed on every read.
    pub(crate) async fn stamp_context_fill_pct(&self, sessions: &mut [Session]) {
        let active = self.active.read().await;
        for session in sessions.iter_mut() {
            rehydrate_context_budget_projection(session);
            session.context_fill_pct = active.get(&session.id).map_or_else(
                || super::monitor::context_fill_pct_from_persisted(session),
                super::monitor::context_fill_pct_for_tracked,
            );
        }
    }

    pub async fn get_session(&self, session_id: Uuid) -> Option<Session> {
        get_session_snapshot(&self.active, &self.completed, &self.store, session_id).await
    }

    /// List all sessions (active and completed).
    ///
    /// RSI-006: eval-replay rows (`is_eval=true`) are excluded by default —
    /// they belong to the rsi-eval harness, not the user's TUI. The
    /// `rsi-eval --inspect` mode is the only documented consumer that should
    /// see them, and it goes through `list_sessions_including_eval` below.
    pub async fn list_sessions(&self) -> Vec<Session> {
        self.list_sessions_filtered(false).await
    }

    /// List all sessions including eval-replay rows. Used by the `rsi-eval`
    /// inspector and tests that need the full row set.
    pub async fn list_sessions_including_eval(&self) -> Vec<Session> {
        self.list_sessions_filtered(true).await
    }

    async fn list_sessions_filtered(&self, include_eval: bool) -> Vec<Session> {
        let mut sessions = Vec::new();
        let mut seen_ids = HashSet::new();

        for tracked in self.active.read().await.values() {
            if !is_visible_in_session_list(&tracked.session) {
                continue;
            }
            if !include_eval && tracked.session.is_eval {
                continue;
            }
            let mut s = tracked.session.clone();
            project_approval_started_at(&mut s, tracked);
            seen_ids.insert(s.id);
            sessions.push(s);
        }

        for completed in self.completed.read().await.values() {
            if !is_visible_in_session_list(&completed.session) {
                continue;
            }
            if !include_eval && completed.session.is_eval {
                continue;
            }
            seen_ids.insert(completed.session.id);
            sessions.push(completed.session.clone());
        }

        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_sessions()
        })
        .await
        {
            Ok(Ok(store_sessions)) => {
                for session in store_sessions {
                    if seen_ids.contains(&session.id) {
                        continue;
                    }
                    if !include_eval && session.is_eval {
                        continue;
                    }
                    sessions.push(session);
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "list_sessions store fallback failed");
            }
            Err(e) => {
                tracing::warn!(error = %e, "list_sessions store fallback task failed");
            }
        }

        self.stamp_context_fill_pct(&mut sessions).await;
        sessions
    }

    /// Get conversation events for a session.
    pub async fn get_conversation(&self, session_id: Uuid) -> Result<Vec<ConversationEvent>> {
        self.get_conversation_since(session_id, None).await
    }

    /// Ensure an owned [`CompletedSession`] carries its real transcript,
    /// loading it from SQLite exactly once if it is still the restore-time
    /// hydration placeholder (`events_hydrated == false`). No-op otherwise.
    ///
    /// Callers that pull a `CompletedSession` out of the `completed` map by
    /// value (`continue_session`, rotation resume) MUST call this before
    /// reading `.events` -- a restored-but-never-viewed session's in-memory
    /// copy is empty until hydrated, and the events are about to be carried
    /// forward into a new `TrackedSession` or replayed to a provider.
    pub(super) async fn hydrate_completed_events(&self, cs: &mut CompletedSession) -> Result<()> {
        if cs.events_hydrated {
            return Ok(());
        }
        cs.events = load_completed_events_from_store(&self.store, cs.session.id).await?;
        cs.events_hydrated = true;
        Ok(())
    }

    /// Get conversation events for a session, optionally only those after `since_sequence`.
    /// When `since_sequence` is Some, returns only events with sequence > that value.
    pub async fn get_conversation_since(
        &self,
        session_id: Uuid,
        since_sequence: Option<i32>,
    ) -> Result<Vec<ConversationEvent>> {
        if let Some(tracked) = self.active.read().await.get(&session_id) {
            let events = Self::filter_events_since(&tracked.events, since_sequence);
            Self::log_conversation_fetch(session_id, since_sequence, events.len(), "active");
            return Ok(events);
        }

        // C7 Phase 1: a restored completed session may still carry the
        // restore-time hydration placeholder (`events_hydrated == false`,
        // `events` empty) -- see `SessionManager::restore_sessions`. Fast path
        // below never touches the store; the slow path loads the real
        // transcript once and writes it back into the cache so repeated polls
        // of a VISIBLE session (the hot case this path serves) stay fast
        // afterward instead of re-hitting SQLite every poll.
        let needs_hydration = matches!(
            self.completed.read().await.get(&session_id),
            Some(completed) if !completed.events_hydrated
        );
        if needs_hydration {
            let events = load_completed_events_from_store(&self.store, session_id).await?;
            let mut completed_guard = self.completed.write().await;
            if let Some(completed) = completed_guard.get_mut(&session_id)
                && !completed.events_hydrated
            {
                completed.events = events;
                completed.events_hydrated = true;
            }
        }
        if let Some(completed) = self.completed.read().await.get(&session_id) {
            let events = Self::filter_events_since(&completed.events, since_sequence);
            Self::log_conversation_fetch(session_id, since_sequence, events.len(), "completed");
            return Ok(events);
        }
        let store = self.store.clone();
        let events = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_events_since(session_id, since_sequence)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        if events.is_empty() {
            Self::log_conversation_fetch(session_id, since_sequence, 0, "store");
            Err(DaemonError::SessionNotFound(session_id))
        } else {
            Self::log_conversation_fetch(session_id, since_sequence, events.len(), "store");
            Ok(events)
        }
    }

    /// Load one bounded, stable page of persisted daemon diagnostics for a
    /// session. Diagnostics are deliberately separate from conversation
    /// sequence numbers.
    pub async fn get_session_diagnostics(
        &self,
        session_id: Uuid,
        after_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<SessionDiagnosticV1>> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.list_session_diagnostics(session_id, after_id, limit)
        })
        .await
        .map_err(|error| DaemonError::Store(error.to_string()))?
    }

    /// Filter in-memory events by sequence threshold.
    fn filter_events_since(
        events: &[ConversationEvent],
        since_sequence: Option<i32>,
    ) -> Vec<ConversationEvent> {
        match since_sequence {
            None => events.to_vec(),
            Some(seq) => events
                .iter()
                .filter(|e| e.sequence > seq)
                .cloned()
                .collect(),
        }
    }

    fn log_conversation_fetch(
        session_id: Uuid,
        since_sequence: Option<i32>,
        returned: usize,
        source: &'static str,
    ) {
        if profiling::enabled() {
            tracing::trace!(
                target = "rsid::profile",
                session_id = %session_id,
                since = since_sequence.unwrap_or(-1),
                returned,
                source,
                "get_conversation_since"
            );
        }
    }

    /// Get turn metrics for a session.
    /// Checks active sessions first, then completed, then falls back to database.
    pub async fn get_turn_metrics(&self, session_id: Uuid) -> Result<Vec<TurnMetric>> {
        // Check active sessions (freshest data)
        if let Some(tracked) = self.active.read().await.get(&session_id) {
            return Ok(tracked.turn_metrics.clone());
        }
        // Check completed sessions (in-memory cache)
        if let Some(completed) = self.completed.read().await.get(&session_id) {
            return Ok(completed.turn_metrics.clone());
        }
        // Fall back to database (for sessions not in memory)
        let store = self.store.clone();
        let metrics = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_turn_metrics(session_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        Ok(metrics)
    }

    /// List archived sessions, optionally filtered by project_id.
    /// Archived sessions are not kept in memory, so this queries the database directly.
    pub async fn list_archived_sessions(&self, project_id: Option<Uuid>) -> Result<Vec<Session>> {
        let store = self.store.clone();
        let sessions = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_archived_sessions(project_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        let mut sessions = sessions;
        self.stamp_context_fill_pct(&mut sessions).await;
        Ok(sessions)
    }

    /// Unarchive a session: restore it to Completed status, load its events and
    /// turn_metrics into memory, and publish a SessionUnarchived event.
    pub async fn unarchive_session(&self, session_id: Uuid) -> Result<Session> {
        // Guard: reject if session is active (shouldn't happen, but be safe)
        if self.active.read().await.contains_key(&session_id) {
            return Err(DaemonError::Rpc("Session is currently active".to_string()));
        }

        let store = self.store.clone();
        let archived = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let session = store.get_session(session_id)?;
            if session.as_ref().is_some_and(|session| {
                matches!(
                    (session.sandbox_kind, session.sandbox_cleanup_state),
                    (
                        Some(SandboxKind::GitWorktree),
                        Some(SandboxCleanupState::Purged)
                    )
                )
            }) {
                store.verify_archive_cleanup_unarchive_gate(session_id)?;
            }
            Ok::<_, DaemonError>(session)
        })
        .await
        .map_err(|error| DaemonError::Store(error.to_string()))??
        .ok_or(DaemonError::SessionNotFound(session_id))?;

        // A historically purged sandbox has no usable worktree. Recreate a
        // clean worktree before exposing the session as completed so a later
        // Continue can authenticate live custody rather than fail with
        // `historical_purged`.
        let mut session = if matches!(
            (archived.sandbox_kind, archived.sandbox_cleanup_state),
            (
                Some(SandboxKind::GitWorktree),
                Some(SandboxCleanupState::Purged)
            )
        ) {
            let sandbox_base = self.sandbox_allocator.base_dir().to_path_buf();
            let archived_for_restore = archived.clone();
            let (allocation, binding) = tokio::task::spawn_blocking(move || {
                fresh_unarchive_sandbox_binding(&archived_for_restore, sandbox_base)
            })
            .await
            .map_err(|error| DaemonError::Store(error.to_string()))??;
            let restored = {
                let mut store = self.store.lock().await;
                store.restore_archived_session_with_fresh_custody(session_id, binding)
            };
            match restored {
                Ok(session) => session,
                Err(error) => {
                    // D00 does not permit cleanup of an unbound worktree. Its
                    // allocated UUID root is retained for startup custody
                    // reconciliation rather than attempting an unproven
                    // destructive rollback here.
                    tracing::warn!(
                        %session_id,
                        sandbox_root = %allocation.root.display(),
                        error = %error,
                        "retaining unbound replacement sandbox after unarchive rejection"
                    );
                    return Err(error);
                }
            }
        } else {
            self.persistence
                .unarchive_session(session_id)
                .await?
                .ok_or(DaemonError::SessionNotFound(session_id))?
        };
        session.context_fill_pct = super::monitor::context_fill_pct_from_persisted(&session);

        // Load events and turn_metrics from database
        let store = self.store.clone();
        let sid = session_id;
        let (events, turn_metrics) = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let events = store.load_events(sid)?;
            let turn_metrics = store.load_turn_metrics(sid)?;
            Ok::<_, DaemonError>((events, turn_metrics))
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        // Insert into completed map
        self.completed.write().await.insert(
            session_id,
            CompletedSession {
                session: session.clone(),
                events,
                turn_metrics,
                retry_cancel: None,
                retry_fired_at: None,
                superseded_by_retry: None,
                events_hydrated: true,
            },
        );

        if session.session_kind == rsi_common::types::SessionKind::Epic
            && session.lead_session_id.is_some()
        {
            self.repair_invalid_epic_leads_on_restore().await?;
        }

        // Publish event
        self.event_bus
            .publish(crate::bus::DaemonEvent::SessionUnarchived { session_id });

        Ok(session)
    }

    /// List deleted (soft-deleted) sessions, optionally filtered by project_id.
    pub async fn list_deleted_sessions(&self, project_id: Option<Uuid>) -> Result<Vec<Session>> {
        let store = self.store.clone();
        let sessions = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_deleted_sessions(project_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        let mut sessions = sessions;
        self.stamp_context_fill_pct(&mut sessions).await;
        Ok(sessions)
    }

    /// Undelete a session: restore it to Completed status, load its data into memory.
    pub async fn undelete_session(&self, session_id: Uuid) -> Result<Session> {
        if self.active.read().await.contains_key(&session_id) {
            return Err(DaemonError::Rpc("Session is currently active".to_string()));
        }

        let mut session = self
            .persistence
            .undelete_session(session_id)
            .await?
            .ok_or(DaemonError::SessionNotFound(session_id))?;
        session.context_fill_pct = super::monitor::context_fill_pct_from_persisted(&session);

        // Load events and turn_metrics from database
        let store = self.store.clone();
        let sid = session_id;
        let (events, turn_metrics) = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let events = store.load_events(sid)?;
            let turn_metrics = store.load_turn_metrics(sid)?;
            Ok::<_, DaemonError>((events, turn_metrics))
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        self.completed.write().await.insert(
            session_id,
            CompletedSession {
                session: session.clone(),
                events,
                turn_metrics,
                retry_cancel: None,
                retry_fired_at: None,
                superseded_by_retry: None,
                events_hydrated: true,
            },
        );

        if session.session_kind == rsi_common::types::SessionKind::Epic
            && session.lead_session_id.is_some()
        {
            self.repair_invalid_epic_leads_on_restore().await?;
        }

        self.event_bus
            .publish(crate::bus::DaemonEvent::SessionUnarchived { session_id });

        Ok(session)
    }

    /// Return daemon health metrics for observability.
    pub async fn get_health_status(&self) -> rsi_common::rpc::HealthStatusResponse {
        let project_cache_size = self.project_index.read().await.len();

        // Fetch queue metrics and rate-limit windows with try_lock to avoid
        // blocking if store is busy. Both are advisory telemetry: a busy store
        // yields an empty reading, never a stalled health call.
        let (queue_metrics, rate_limits) = if let Ok(store) = self.store.try_lock() {
            (
                store.queue_metrics().ok(),
                store
                    .load_provider_rate_limit_snapshots()
                    .unwrap_or_default(),
            )
        } else {
            (None, Vec::new())
        };

        rsi_common::rpc::HealthStatusResponse {
            persistence_queue_depth: self.persistence.pending.load(Ordering::Relaxed),
            persistence_queue_capacity: self.persistence.capacity,
            last_command_duration_ms: self
                .persistence
                .last_command_duration_ms
                .load(Ordering::Relaxed),
            project_cache_size,
            rate_limits,
            project_cache_hits: 0,
            project_cache_misses: 0,
            last_poll_payload_bytes: 0,
            last_poll_event_count: 0,
            provider_claude_available: self.claude_client.is_some(),
            provider_codex_available: self.codex_client.is_some(),
            provider_pioneer_available: crate::pioneer::pioneer_provider_available(
                self.codex_client.is_some(),
            ),
            provider_bedrock_available: crate::bedrock::available(self.codex_client.is_some()),
            provider_openrouter_available: crate::openrouter::openrouter_provider_available(
                self.codex_client.is_some(),
            ),
            provider_local_available: self.local_client.is_some(),
            provider_antigravity_available: self.agy_client.is_some(),
            provider_harness_available: true,
            provider_codex_app_server_available:
                crate::codex_app_server::CodexAppServerClient::is_available(),
            queue_pending: queue_metrics.as_ref().map(|m| m.pending).unwrap_or(0),
            queue_claimed: queue_metrics.as_ref().map(|m| m.claimed).unwrap_or(0),
            queue_completed: queue_metrics.as_ref().map(|m| m.completed).unwrap_or(0),
            queue_failed: queue_metrics.as_ref().map(|m| m.failed).unwrap_or(0),
            latest_daemon_restart: self.latest_daemon_restart.clone(),
        }
    }

    /// List sessions filtered by project.
    /// Combines in-memory active/completed sessions with project filter.
    pub async fn list_sessions_by_project(&self, project_id: Option<Uuid>) -> Vec<Session> {
        let mut sessions = Vec::new();
        let mut seen_ids = HashSet::new();

        // Filter active sessions
        for tracked in self.active.read().await.values() {
            if !is_visible_in_session_list(&tracked.session) {
                continue;
            }
            if tracked.session.project_id == project_id {
                let mut s = tracked.session.clone();
                project_approval_started_at(&mut s, tracked);
                seen_ids.insert(s.id);
                sessions.push(s);
            }
        }

        // Filter completed sessions
        for completed in self.completed.read().await.values() {
            if !is_visible_in_session_list(&completed.session) {
                continue;
            }
            if completed.session.project_id == project_id {
                seen_ids.insert(completed.session.id);
                sessions.push(completed.session.clone());
            }
        }

        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_sessions_by_project(project_id)
        })
        .await
        {
            Ok(Ok(store_sessions)) => {
                for session in store_sessions {
                    if seen_ids.insert(session.id) {
                        sessions.push(session);
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    project_id = ?project_id,
                    error = %e,
                    "list_sessions_by_project store fallback failed"
                );
            }
            Err(e) => {
                tracing::warn!(
                    project_id = ?project_id,
                    error = %e,
                    "list_sessions_by_project store fallback task failed"
                );
            }
        }

        self.stamp_context_fill_pct(&mut sessions).await;
        sessions
    }
}
