//! IssueTrackerManager: owns the polling loop and dispatch state.

use super::poller::{self, PollerState, SessionLauncher};
use super::tracker::Tracker;
use super::types::{DispatchRecord, IssueTrackerConfig, IssueTrackerStatus, TickResult};
use crate::bus::{DaemonEvent, EventBus};
use crate::error::Result;
use crate::store::Store;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Terminal predicate for issue-dispatch bookkeeping: true for ANY status
/// that means the session's process is gone and its `running` slot must be
/// freed. Deliberately broader than `SessionStatus::is_terminal()`
/// (`crates/rsi-common/src/types.rs`), which excludes `Deleted` because other
/// call sites (DAG heartbeats, graph waits) treat a deleted session as
/// possibly recoverable. For dispatch bookkeeping a deleted session's
/// subprocess is gone regardless, so `Deleted` counts as terminal here too.
fn is_dispatch_terminal_status(status: rsi_common::types::SessionStatus) -> bool {
    status.is_terminal() || matches!(status, rsi_common::types::SessionStatus::Deleted)
}

/// Persisted terminal reason string for a non-Completed terminal status.
/// `Completed` itself is handled separately via `handle_session_completed`,
/// which persists `"completed"`.
const fn terminal_reason(status: rsi_common::types::SessionStatus) -> &'static str {
    use rsi_common::types::SessionStatus;
    match status {
        SessionStatus::Failed => "failed",
        SessionStatus::Interrupted => "interrupted",
        SessionStatus::Archived => "archived",
        SessionStatus::Deleted => "deleted",
        // Completed is routed to handle_session_completed, and is_dispatch_terminal_status
        // gates this function to the terminal set above; unreachable in practice.
        _ => "terminal",
    }
}

pub struct IssueTrackerManager {
    config: IssueTrackerConfig,
    tracker: Box<dyn Tracker>,
    state: Mutex<PollerState>,
    event_bus: Arc<EventBus>,
    session_launcher: Arc<dyn SessionLauncher>,
    store: Arc<Mutex<Store>>,
    last_poll_at: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
}

impl IssueTrackerManager {
    pub fn new(
        config: IssueTrackerConfig,
        tracker: Box<dyn Tracker>,
        event_bus: Arc<EventBus>,
        session_launcher: Arc<dyn SessionLauncher>,
        store: Arc<Mutex<Store>>,
    ) -> Self {
        Self {
            config,
            tracker,
            state: Mutex::new(PollerState::new()),
            event_bus,
            session_launcher,
            store,
            last_poll_at: Mutex::new(None),
        }
    }

    /// Spawn the background polling loop. Returns a JoinHandle.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_millis(self.config.poll_interval_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;
                // Acquire state mutex for the entire tick to prevent double-dispatch
                let mut state = self.state.lock().await;
                if let Err(e) = poller::tick(
                    self.tracker.as_ref(),
                    &self.config,
                    &mut state,
                    self.session_launcher.as_ref(),
                    &self.store,
                    &self.event_bus,
                )
                .await
                {
                    tracing::error!(error = %e, "Issue tracker tick failed");
                    self.event_bus.publish(DaemonEvent::SystemMessage {
                        level: "error".to_string(),
                        message: format!("Issue tracker poll failed: {}", e),
                    });
                }
                *self.last_poll_at.lock().await = Some(chrono::Utc::now());
            }
        })
    }

    /// Spawn a completion listener that watches issue-driven sessions for
    /// ANY terminal status and closes out the dispatch slot accordingly.
    ///
    /// Only `Completed` drives the Linear-update side effect
    /// (`handle_session_completed`); every other terminal status
    /// (`Failed`/`Interrupted`/`Archived`/`Deleted`) is closed out via
    /// `handle_session_terminal_non_completed`, which just frees the
    /// `state.running` slot and marks the dispatch row terminal in the
    /// store. Without this, a dispatch whose session ends any way other than
    /// `Completed` never leaves `state.running` and permanently occupies a
    /// `max_concurrent` slot.
    pub fn spawn_completion_listener(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut rx = this.event_bus.subscribe();
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if let DaemonEvent::SessionStatusChanged {
                            session_id,
                            new_status,
                            ..
                        } = &*event
                        {
                            if *new_status == rsi_common::types::SessionStatus::Completed {
                                this.handle_session_completed(*session_id).await;
                            } else if is_dispatch_terminal_status(*new_status) {
                                this.handle_session_terminal_non_completed(
                                    *session_id,
                                    *new_status,
                                )
                                .await;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(
                            missed,
                            "issue tracker completion listener lagged on the event bus; \
                             re-checking running dispatches for completion directly"
                        );
                        this.recheck_running_dispatches_for_completion().await;
                    }
                }
            }
            this.event_bus.unsubscribe();
        })
    }

    /// Catch-up after a broadcast lag: a lagged receiver may have missed the
    /// `SessionStatusChanged` event entirely (broadcast does not redeliver
    /// skipped messages), so directly re-check every currently running
    /// dispatch's session status against the store and handle any session
    /// that reached a terminal status while we were lagging. Uses the same
    /// `is_dispatch_terminal_status` predicate as the live listener above.
    async fn recheck_running_dispatches_for_completion(&self) {
        let session_ids: Vec<Uuid> = {
            let state = self.state.lock().await;
            state.running.values().map(|d| d.session_id).collect()
        };
        for session_id in session_ids {
            let session = {
                let store = self.store.lock().await;
                store.get_session(session_id)
            };
            match session {
                Ok(Some(session))
                    if session.status == rsi_common::types::SessionStatus::Completed =>
                {
                    self.handle_session_completed(session_id).await;
                }
                Ok(Some(session)) if is_dispatch_terminal_status(session.status) => {
                    self.handle_session_terminal_non_completed(session_id, session.status)
                        .await;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        %session_id,
                        "Failed to re-check session status after event bus lag"
                    );
                }
            }
        }
    }

    /// Close out a dispatch whose session reached a terminal status other
    /// than `Completed`. Deliberately narrower than `handle_session_completed`:
    /// no Linear/tracker update is invoked (that side effect is
    /// Completed-only by design) — just free the `state.running` slot and
    /// persist the terminal reason so `restore_from_db` never re-adopts it
    /// after a daemon restart.
    async fn handle_session_terminal_non_completed(
        &self,
        session_id: Uuid,
        status: rsi_common::types::SessionStatus,
    ) {
        let issue_id = {
            let state = self.state.lock().await;
            state
                .running
                .values()
                .find(|d| d.session_id == session_id)
                .map(|d| d.issue_id.clone())
        };

        let Some(issue_id) = issue_id else {
            return; // Not an issue-driven session
        };

        let dispatch = {
            let mut state = self.state.lock().await;
            state.running.remove(&issue_id)
        };

        let Some(dispatch) = dispatch else {
            return; // Removed concurrently between the lookup and the remove
        };

        let reason = terminal_reason(status);
        tracing::info!(
            issue = %dispatch.issue_identifier,
            session_id = %session_id,
            status = ?status,
            "Closing out issue dispatch: session reached a non-completed terminal status"
        );

        let store = self.store.lock().await;
        if let Err(e) = store.mark_issue_dispatch_terminal(&dispatch.issue_id, reason) {
            tracing::error!(
                error = %e,
                issue = %dispatch.issue_identifier,
                "Failed to mark issue dispatch terminal"
            );
        }
    }

    /// Handle a completed session: check if it's issue-driven and update tracker state.
    async fn handle_session_completed(&self, session_id: Uuid) {
        let completion_state = match &self.config.completion_state {
            Some(state) => state.clone(),
            None => return,
        };

        // Find the dispatch record for this session
        let dispatch = {
            let state = self.state.lock().await;
            state
                .running
                .values()
                .find(|d| d.session_id == session_id)
                .cloned()
        };

        let Some(dispatch) = dispatch else {
            return; // Not an issue-driven session
        };

        if self.config.kind == "local" {
            let project_id = match self.config.project_id {
                Some(project_id) => project_id,
                None => {
                    tracing::error!("Local issue completion missing project binding");
                    return;
                }
            };
            let matches = match self
                .store
                .lock()
                .await
                .local_issue_dispatch_matches_project(&dispatch.issue_id, session_id, project_id)
            {
                Ok(matches) => matches,
                Err(error) => {
                    tracing::error!(error = %error, "Local issue completion ownership check failed");
                    return;
                }
            };
            if !matches {
                let message = format!(
                    "Local issue completion ownership mismatch for {} and session {}",
                    dispatch.issue_identifier, session_id
                );
                tracing::error!("{message}");
                self.event_bus.publish(DaemonEvent::SystemMessage {
                    level: "error".to_string(),
                    message,
                });
                return;
            }
        }

        tracing::info!(
            issue = %dispatch.issue_identifier,
            session_id = %session_id,
            target_state = %completion_state,
            "Updating Linear issue state on session completion"
        );

        if let Err(e) = self
            .tracker
            .update_issue_state(&self.config, &dispatch.issue_id, &completion_state)
            .await
        {
            tracing::error!(
                error = %e,
                issue = %dispatch.issue_identifier,
                "Failed to update Linear issue state"
            );
        }

        // Remove from running and mark terminal
        {
            let mut state = self.state.lock().await;
            state.running.remove(&dispatch.issue_id);
        }
        {
            let store = self.store.lock().await;
            let _ = store.mark_issue_dispatch_terminal(&dispatch.issue_id, "completed");
        }
    }

    /// Restore state from SQLite on startup.
    ///
    /// F1 (Track C slice C3, review obligation carried from C2): gate the
    /// insert on `dispatch.tracker == self.config.kind` in addition to
    /// non-terminal, so a daemon running one tracker kind never adopts
    /// another kind's dispatch rows into its in-memory `running`/`claimed`
    /// state. Non-mutating — no dispatch row is touched either way; a
    /// cross-kind row is simply left out of this process's state and stays
    /// exactly as persisted. Pre-C2, every row was `tracker="linear"`
    /// (column default), so `kind="linear"` restores byte-for-byte what it
    /// always restored.
    pub async fn restore_from_db(&self) -> Result<()> {
        let store = self.store.lock().await;
        let dispatches = if self.config.kind == "local" {
            let project_id = self.config.project_id.ok_or_else(|| {
                crate::error::DaemonError::InvalidParam(
                    "local issue tracker project binding is required".to_string(),
                )
            })?;
            store.load_active_dispatches_for_tracker_project("local", project_id)?
        } else {
            store.load_active_dispatches()?
        };
        let mut state = self.state.lock().await;
        for dispatch in dispatches {
            if dispatch.terminal_state.is_none() && dispatch.tracker == self.config.kind {
                state.claimed.insert(dispatch.issue_id.clone());
                state.running.insert(dispatch.issue_id.clone(), dispatch);
            }
        }
        Ok(())
    }

    /// Trigger an immediate poll (for manual override via RPC).
    pub async fn trigger_poll(&self) -> Result<TickResult> {
        let mut state = self.state.lock().await;
        let result = poller::tick(
            self.tracker.as_ref(),
            &self.config,
            &mut state,
            self.session_launcher.as_ref(),
            &self.store,
            &self.event_bus,
        )
        .await?;
        *self.last_poll_at.lock().await = Some(chrono::Utc::now());
        Ok(result)
    }

    /// Get current status snapshot (for RPC).
    pub async fn status(&self) -> IssueTrackerStatus {
        let state = self.state.lock().await;
        let last_poll = *self.last_poll_at.lock().await;
        let next_poll = last_poll
            .map(|lp| lp + chrono::Duration::milliseconds(self.config.poll_interval_ms as i64));
        IssueTrackerStatus {
            enabled: true,
            tracker: self.config.kind.clone(),
            last_poll_at: last_poll,
            next_poll_at: next_poll,
            dispatched_count: state.running.len(),
            max_concurrent: self.config.max_concurrent,
            poll_interval_ms: self.config.poll_interval_ms,
            active_states: self.config.active_states.clone(),
        }
    }

    /// Get list of currently dispatched issues (for RPC).
    pub async fn dispatched_issues(&self) -> Vec<DispatchRecord> {
        let state = self.state.lock().await;
        state.running.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::TrackedIssue;
    use super::*;
    use crate::claude::LaunchConfig;
    use crate::store::Store;
    use async_trait::async_trait;

    /// Tracker stub — `restore_from_db` never calls the tracker, so every
    /// method here is unreachable in these tests; it exists only to satisfy
    /// `IssueTrackerManager::new`'s `Box<dyn Tracker>` parameter.
    struct NoopTracker;

    #[async_trait]
    impl Tracker for NoopTracker {
        async fn fetch_candidates(
            &self,
            _config: &IssueTrackerConfig,
        ) -> Result<Vec<TrackedIssue>> {
            Ok(Vec::new())
        }
        async fn fetch_by_ids(
            &self,
            _config: &IssueTrackerConfig,
            _ids: &[String],
        ) -> Result<Vec<TrackedIssue>> {
            Ok(Vec::new())
        }
        async fn update_issue_state(
            &self,
            _config: &IssueTrackerConfig,
            _issue_id: &str,
            _state_name: &str,
        ) -> Result<()> {
            Ok(())
        }
        async fn resolve_viewer_id(&self, _config: &IssueTrackerConfig) -> Result<String> {
            Ok("test".to_string())
        }
    }

    /// Launcher stub — `restore_from_db` never dispatches a session, so
    /// `launch` is unreachable in these tests; exists only to satisfy
    /// `IssueTrackerManager::new`'s `Arc<dyn SessionLauncher>` parameter.
    struct NoopLauncher;

    #[async_trait]
    impl SessionLauncher for NoopLauncher {
        async fn launch(&self, _config: LaunchConfig) -> Result<Uuid> {
            Ok(Uuid::new_v4())
        }
    }

    fn make_dispatch(issue_id: &str, tracker: &str) -> DispatchRecord {
        DispatchRecord {
            issue_id: issue_id.to_string(),
            issue_identifier: format!("{tracker}-{issue_id}"),
            tracker: tracker.to_string(),
            session_id: Uuid::new_v4(),
            dispatched_at: chrono::Utc::now(),
            last_reconciled_at: None,
            terminal_state: None,
        }
    }

    fn make_manager(kind: &str, store: Arc<Mutex<Store>>) -> IssueTrackerManager {
        let config = IssueTrackerConfig {
            kind: kind.to_string(),
            project_id: (kind == "local").then(crate::store::d04_test_project_id),
            ..IssueTrackerConfig::default()
        };
        IssueTrackerManager::new(
            config,
            Box::new(NoopTracker),
            Arc::new(EventBus::new(16)),
            Arc::new(NoopLauncher),
            store,
        )
    }

    /// F1 regression: seed a `linear` AND a `local` active dispatch;
    /// `restore_from_db` under `config.kind="local"` must land only the
    /// local row in `running`/`claimed` — the linear row is left untouched
    /// in the DB (non-mutating) and simply absent from this process's
    /// in-memory state.
    #[tokio::test]
    async fn restore_from_db_filters_cross_kind_dispatches_under_local_kind() {
        let store = Store::open_in_memory().unwrap();
        let mut session = crate::store::tests::make_test_session();
        session.project_id = Some(crate::store::d04_test_project_id());
        store.insert_session(&session).unwrap();
        let issue = store
            .create_issue(&rsi_common::types::NewIssue {
                project_id: crate::store::d04_test_project_id(),
                title: "local restore".to_string(),
                body: String::new(),
                priority: None,
                labels: Vec::new(),
                created_by_session_id: Some(session.id),
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap();
        store
            .insert_issue_dispatch(&make_dispatch("linear-1", "linear"))
            .unwrap();
        store
            .insert_issue_dispatch(&DispatchRecord {
                issue_id: issue.id.to_string(),
                issue_identifier: "LOCAL-restore".to_string(),
                tracker: "local".to_string(),
                session_id: session.id,
                dispatched_at: chrono::Utc::now(),
                last_reconciled_at: None,
                terminal_state: None,
            })
            .unwrap();
        let store = Arc::new(Mutex::new(store));

        let manager = make_manager("local", Arc::clone(&store));
        manager.restore_from_db().await.unwrap();

        let dispatched = manager.dispatched_issues().await;
        assert_eq!(
            dispatched.len(),
            1,
            "only the local-kind dispatch should be restored"
        );
        assert_eq!(dispatched[0].issue_id, issue.id.to_string());
        assert_eq!(dispatched[0].tracker, "local");
    }

    /// Inverse of the above (symmetry check, not a `local`-only special
    /// case): under `config.kind="linear"`, only the linear row restores.
    /// Pre-C2 every dispatch row was `tracker="linear"` (column default),
    /// so this also pins byte-for-byte prior behavior for the existing
    /// Linear-only deployment shape.
    #[tokio::test]
    async fn restore_from_db_filters_cross_kind_dispatches_under_linear_kind() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_issue_dispatch(&make_dispatch("linear-1", "linear"))
            .unwrap();
        store
            .insert_issue_dispatch(&make_dispatch("local-1", "local"))
            .unwrap();
        let store = Arc::new(Mutex::new(store));

        let manager = make_manager("linear", Arc::clone(&store));
        manager.restore_from_db().await.unwrap();

        let dispatched = manager.dispatched_issues().await;
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].issue_id, "linear-1");
        assert_eq!(dispatched[0].tracker, "linear");
    }

    /// Minimal `Completed` session for the lag-catch-up regression test
    /// below. Field list mirrors `store::tests::make_test_session`.
    fn make_completed_session(id: Uuid) -> rsi_common::types::Session {
        use rsi_common::types::{ContextUsageConfidence, Session, SessionProvider, SessionStatus};
        Session {
            context_fill_pct: None,
            id,
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: std::path::PathBuf::from("/tmp/test"),
            git_branch: None,
            status: SessionStatus::Completed,
            project_id: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            session_kind: rsi_common::types::SessionKind::Standard,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            continued_from: None,
            context_usage_confidence: ContextUsageConfidence::Missing,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
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

    /// Regression test for the broadcast-bus-lag silent-drop defect
    /// (`fix/bus-lag-silent-drop`, part (a)): `spawn_completion_listener`
    /// used to do `Err(RecvError::Lagged(_)) => continue` with no catch-up.
    /// A lagged receiver can miss the `SessionStatusChanged { Completed }`
    /// event outright — broadcast does not redeliver skipped messages —
    /// leaving the dispatch running forever and its tracker issue never
    /// closed until daemon restart.
    ///
    /// This exercises `recheck_running_dispatches_for_completion` directly
    /// (the catch-up the `Lagged` arm now calls) against a dispatch whose
    /// backing session already completed in the store, and asserts it gets
    /// closed out exactly as the live event would have closed it out.
    #[tokio::test]
    async fn recheck_running_dispatches_for_completion_closes_out_completed_session() {
        let store = Store::open_in_memory().unwrap();
        let session_id = Uuid::new_v4();
        store
            .insert_session(&make_completed_session(session_id))
            .unwrap();

        let mut dispatch = make_dispatch("linear-1", "linear");
        dispatch.session_id = session_id;
        store.insert_issue_dispatch(&dispatch).unwrap();

        let store = Arc::new(Mutex::new(store));
        let config = IssueTrackerConfig {
            kind: "linear".to_string(),
            completion_state: Some("Done".to_string()),
            ..IssueTrackerConfig::default()
        };

        let manager = IssueTrackerManager::new(
            config,
            Box::new(NoopTracker),
            Arc::new(EventBus::new(16)),
            Arc::new(NoopLauncher),
            Arc::clone(&store),
        );
        manager.restore_from_db().await.unwrap();
        assert_eq!(
            manager.dispatched_issues().await.len(),
            1,
            "dispatch should be restored as running before the recheck"
        );

        // This is the catch-up the Lagged branch now invokes instead of
        // silently `continue`-ing past a missed completion event.
        manager.recheck_running_dispatches_for_completion().await;

        let dispatched = manager.dispatched_issues().await;
        assert!(
            dispatched.is_empty(),
            "completed session's dispatch must be closed out by the lag catch-up, not left running forever"
        );

        let store = store.lock().await;
        let reloaded = store
            .load_dispatch_by_issue_id("linear-1")
            .unwrap()
            .expect("dispatch row still exists");
        assert_eq!(reloaded.terminal_state.as_deref(), Some("completed"));
    }

    /// Regression test for the terminal-status leak (finding N1,
    /// `thoughts/shared/reviews/2026-07-31-bus-lag-silent-drop-review.md`):
    /// `recheck_running_dispatches_for_completion` used to match ONLY
    /// `SessionStatus::Completed`, so a dispatch whose session ended
    /// `Failed`/`Interrupted`/`Archived`/`Deleted` was never closed out and
    /// permanently occupied a `max_concurrent` slot. This exercises the
    /// catch-up path directly against a `Failed` session and asserts it gets
    /// closed out (removed from `running`, persisted with a non-"completed"
    /// terminal reason) exactly like a `Completed` session already was.
    #[tokio::test]
    async fn recheck_running_dispatches_for_completion_closes_out_failed_session() {
        let store = Store::open_in_memory().unwrap();
        let session_id = Uuid::new_v4();
        let mut session = make_completed_session(session_id);
        session.status = rsi_common::types::SessionStatus::Failed;
        store.insert_session(&session).unwrap();

        let mut dispatch = make_dispatch("linear-1", "linear");
        dispatch.session_id = session_id;
        store.insert_issue_dispatch(&dispatch).unwrap();

        let store = Arc::new(Mutex::new(store));
        let config = IssueTrackerConfig {
            kind: "linear".to_string(),
            completion_state: Some("Done".to_string()),
            ..IssueTrackerConfig::default()
        };

        let manager = IssueTrackerManager::new(
            config,
            Box::new(NoopTracker),
            Arc::new(EventBus::new(16)),
            Arc::new(NoopLauncher),
            Arc::clone(&store),
        );
        manager.restore_from_db().await.unwrap();
        assert_eq!(
            manager.dispatched_issues().await.len(),
            1,
            "dispatch should be restored as running before the recheck"
        );

        manager.recheck_running_dispatches_for_completion().await;

        let dispatched = manager.dispatched_issues().await;
        assert!(
            dispatched.is_empty(),
            "failed session's dispatch must be closed out by the lag catch-up, \
             not left running forever occupying a max_concurrent slot"
        );

        let store = store.lock().await;
        let reloaded = store
            .load_dispatch_by_issue_id("linear-1")
            .unwrap()
            .expect("dispatch row still exists");
        assert_eq!(reloaded.terminal_state.as_deref(), Some("failed"));
    }

    /// WIRING regression test (R3 clause d): drives the fix through the
    /// REAL `spawn_completion_listener` task and the REAL event bus, not an
    /// extracted helper. Publishes `SessionStatusChanged { new_status:
    /// Failed }` for a running dispatch's session on the live broadcast bus
    /// and asserts the dispatch leaves `state.running`. If the listener's
    /// match arm is reverted to matching only `Completed` (the pre-fix
    /// shape), this test fails because the dispatch is never observed by
    /// the listener and stays in `running` forever.
    #[tokio::test]
    async fn spawn_completion_listener_closes_out_failed_session_via_real_event_bus() {
        let store = Store::open_in_memory().unwrap();
        let session_id = Uuid::new_v4();
        // The session row itself only needs to exist for restore_from_db's
        // FK-shaped bookkeeping; the listener path never re-reads it — it
        // reacts to the published event, not the store.
        store
            .insert_session(&make_completed_session(session_id))
            .unwrap();

        let mut dispatch = make_dispatch("linear-1", "linear");
        dispatch.session_id = session_id;
        store.insert_issue_dispatch(&dispatch).unwrap();

        let store = Arc::new(Mutex::new(store));
        let config = IssueTrackerConfig {
            kind: "linear".to_string(),
            completion_state: Some("Done".to_string()),
            ..IssueTrackerConfig::default()
        };
        let event_bus = Arc::new(EventBus::new(16));

        let manager = Arc::new(IssueTrackerManager::new(
            config,
            Box::new(NoopTracker),
            Arc::clone(&event_bus),
            Arc::new(NoopLauncher),
            Arc::clone(&store),
        ));
        manager.restore_from_db().await.unwrap();
        assert_eq!(manager.dispatched_issues().await.len(), 1);

        let listener = manager.spawn_completion_listener();

        // Wait for the listener task to actually subscribe before
        // publishing, so the event isn't emitted into an empty channel.
        let subscribe_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while event_bus.subscriber_count() == 0 {
            if tokio::time::Instant::now() > subscribe_deadline {
                panic!("completion listener never subscribed to the event bus");
            }
            tokio::task::yield_now().await;
        }

        event_bus.publish(DaemonEvent::SessionStatusChanged {
            session_id,
            old_status: rsi_common::types::SessionStatus::Running,
            new_status: rsi_common::types::SessionStatus::Failed,
        });

        let closed_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if manager.dispatched_issues().await.is_empty() {
                break;
            }
            if tokio::time::Instant::now() > closed_deadline {
                panic!(
                    "dispatch was not closed out by the real listener after a Failed \
                     SessionStatusChanged event; it would otherwise occupy a \
                     max_concurrent slot forever"
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let store_guard = store.lock().await;
        let reloaded = store_guard
            .load_dispatch_by_issue_id("linear-1")
            .unwrap()
            .expect("dispatch row still exists");
        assert_eq!(reloaded.terminal_state.as_deref(), Some("failed"));
        drop(store_guard);

        listener.abort();
    }
}
