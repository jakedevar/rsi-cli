//! The tick pipeline: reconcile running issues, fetch candidates, sort, dispatch.

use super::eligibility;
use super::tracker::Tracker;
use super::types::{DispatchRecord, IssueTrackerConfig, TickResult, TrackedIssue};
use crate::bus::{DaemonEvent, EventBus};
use crate::claude::LaunchConfig;
use crate::error::Result;
use crate::model_control::hash_request_fingerprint;
use crate::store::Store;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Outcome of a terminal-watch fire attempt (A8 `WakeMode::OnTerminal`).
///
/// The scheduler maps these to job-row actions (plan §3.4 table):
/// `Delivered` stamps and re-arms every coalesced job pending confirmation,
/// `Confirmed` retires a proven-consumed delivery, `NotReady` advances the
/// recurring row (requeue-until-idle, D3), and `Abandon` /
/// `AbandonUnconsumed` disable + surface a `SystemMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchFireOutcome {
    /// One coalesced wake was delivered by resuming the master lineage tip.
    Delivered {
        /// The resumed (tip) session.
        session_id: Uuid,
        /// Every watch job satisfied by this single delivery — the fired job
        /// plus any fire-ready siblings sharing the same wake target. All of
        /// them must be stamped and re-armed pending confirmation.
        delivered_job_ids: Vec<Uuid>,
        /// Row versions observed while planning this delivery. A child can be
        /// continued while the owner resume is in flight; the scheduler must
        /// not stamp that new watch epoch with this old delivery.
        observed_job_versions: Vec<(Uuid, chrono::DateTime<chrono::Utc>)>,
    },
    /// Watched child not notify-worthy yet, store row lagging the bus event,
    /// or master busy / transient continue failure: stay armed.
    NotReady,
    /// Custody is temporarily fenced by a reclaim intent or busy root stripe.
    /// Preserve even a one-shot watch without stamping delivery.
    CustodyUnavailable,
    /// A prior delivery is now PROVEN consumed — the resumed tip produced
    /// provider output after the attempt. This, not the spawn, is what retires
    /// a watch row.
    Confirmed,
    /// Watched or master session missing/purged, or lineage tip
    /// unresolvable: disable and surface.
    Abandon { reason: String },
    /// The delivery give-up window elapsed without the tip ever producing
    /// provider output (issue #648). Settled like `Abandon` (same log line,
    /// row retirement and `SystemMessage`), with the retirement, the tip's
    /// health fact and any manager notice committed in one transaction.
    AbandonUnconsumed(UnconsumedDelivery),
}

/// Typed give-up fact for a terminal-watch delivery the wake tip never
/// consumed (issue #648). `reason()` renders the historical log/warning text
/// byte-for-byte so the non-manager path is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnconsumedDelivery {
    /// Rotation-lineage tip the delivery was addressed to.
    pub tip: Uuid,
    /// Watched child whose terminal row anchors the give-up clock.
    pub watched: Uuid,
    /// Whole minutes between the watched child's terminal row and give-up.
    pub minutes: i64,
}

impl UnconsumedDelivery {
    #[must_use]
    pub fn reason(&self) -> String {
        let Self {
            tip,
            watched,
            minutes,
        } = self;
        format!(
            "delivery to {tip} was never consumed: no provider output after \
             {minutes} min of re-delivery attempts for watched child {watched}. The \
             notification is being dropped; resume {tip} manually to pick the \
             work back up"
        )
    }
}

/// Thin interface over SessionManager::launch_session for testability.
#[async_trait]
pub trait SessionLauncher: Send + Sync {
    async fn launch(&self, config: LaunchConfig) -> Result<Uuid>;

    /// Launch a Fresh scheduled job with the narrow origin state resolved by
    /// the scheduler. Defaulting to `launch` keeps non-SessionManager users
    /// behaviorally equivalent while the production implementation initializes
    /// the session before its rotation coordinator exists.
    async fn launch_scheduled_fresh(
        &self,
        config: LaunchConfig,
        _initial_rotation_disabled: bool,
    ) -> Result<Uuid> {
        self.launch(config).await
    }

    /// Resume an existing session for a scheduled wake job.
    /// The default impl returns an error; override in SessionManager.
    async fn resume_scheduled(&self, _target: Uuid, _query: String) -> Result<Uuid> {
        Err(crate::error::DaemonError::Rpc(
            "resume_scheduled not implemented for this launcher".into(),
        ))
    }

    /// Resume for exact scheduled job rows. `SessionManager` revalidates the
    /// rows under the target spawn guard; the default keeps other launchers'
    /// existing behavior.
    async fn resume_scheduled_job(
        &self,
        target: Uuid,
        query: String,
        _job_ids: Vec<Uuid>,
    ) -> Result<Uuid> {
        self.resume_scheduled(target, query).await
    }

    async fn resume_capacity_scheduled(
        &self,
        _target: Uuid,
        _query: String,
        _wake_job_id: Uuid,
        _due_slot: chrono::DateTime<chrono::Utc>,
    ) -> Result<Uuid> {
        Err(crate::error::DaemonError::Rpc(
            "resume_capacity_scheduled not implemented for this launcher".into(),
        ))
    }

    /// Fire a terminal-watch job (A8 `WakeMode::OnTerminal`): evaluate the
    /// watched session's persisted state, coalesce fire-ready siblings, and
    /// deliver one wake to the master lineage tip.
    /// The default impl returns an error; override in `SessionManager`.
    async fn fire_watch(&self, _job: &rsi_common::types::ScheduledJob) -> Result<WatchFireOutcome> {
        Err(crate::error::DaemonError::Rpc(
            "fire_watch not implemented for this launcher".into(),
        ))
    }

    /// Lowest transcript sequence a daemon-authored event on `session` may
    /// take so it sorts after the launcher's in-memory transcript (issue
    /// #648). The default has no cache and returns 0.
    async fn transcript_sequence_floor(&self, _session: Uuid) -> i32 {
        0
    }

    /// A committed issue #648 health-fact event: mirror it into the
    /// launcher's in-memory transcript. Called only after the store commit;
    /// the scheduler publishes the bus events. The default does nothing.
    async fn delivery_abandoned_recorded(&self, _event: &rsi_common::types::ConversationEvent) {}
}

/// Mutable state held by the poller across ticks.
pub struct PollerState {
    /// Issues dispatched (ever, this daemon lifetime).
    pub claimed: HashSet<String>,
    /// Currently running issue sessions.
    pub running: HashMap<String, DispatchRecord>,
}

impl PollerState {
    pub fn new() -> Self {
        Self {
            claimed: HashSet::new(),
            running: HashMap::new(),
        }
    }
}

/// Execute one tick of the issue tracker poll cycle.
pub async fn tick(
    tracker: &dyn Tracker,
    config: &IssueTrackerConfig,
    state: &mut PollerState,
    session_launcher: &dyn SessionLauncher,
    store: &Arc<Mutex<Store>>,
    event_bus: &EventBus,
) -> Result<TickResult> {
    let mut result = TickResult {
        issues_found: 0,
        dispatched: 0,
        skipped_claimed: 0,
        skipped_blocked: 0,
        errors: Vec::new(),
    };

    // 1. Reconcile: check running issues for terminal/inactive transitions
    if !state.running.is_empty() {
        let running_ids: Vec<String> = state.running.keys().cloned().collect();
        match tracker.fetch_by_ids(config, &running_ids).await {
            Ok(fetched) => {
                let fetched_map: HashMap<String, TrackedIssue> =
                    fetched.into_iter().map(|i| (i.id.clone(), i)).collect();

                let mut to_remove = Vec::new();
                for (issue_id, dispatch) in &state.running {
                    if let Some(issue) = fetched_map.get(issue_id) {
                        if issue.state.state_type == "completed"
                            || issue.state.state_type == "cancelled"
                        {
                            // Issue moved to terminal state externally
                            to_remove.push((issue_id.clone(), issue.state.state_type.clone()));

                            event_bus.publish(DaemonEvent::IssueReconciled {
                                issue_id: issue_id.clone(),
                                issue_identifier: dispatch.issue_identifier.clone(),
                                old_state: "active".to_string(),
                                new_state: issue.state.state_type.clone(),
                                action: "completed".to_string(),
                            });
                        }
                    }
                }

                for (issue_id, terminal_state) in to_remove {
                    state.running.remove(&issue_id);
                    // Persist terminal state
                    let store = store.lock().await;
                    let _ = store.mark_issue_dispatch_terminal(&issue_id, &terminal_state);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to reconcile running issues");
                result.errors.push(format!("reconciliation: {}", e));
            }
        }
    }

    // 2. Fetch candidate issues
    let mut candidates = match tracker.fetch_candidates(config).await {
        Ok(issues) => issues,
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch issue candidates");
            result.errors.push(format!("fetch: {}", e));
            return Ok(result);
        }
    };
    result.issues_found = candidates.len();

    // 3. Sort by priority
    eligibility::sort_by_priority(&mut candidates);

    // 4. Filter and dispatch
    for issue in &candidates {
        if let Some(reason) = eligibility::check_eligibility(
            issue,
            config,
            &state.claimed,
            &state.running,
            state.running.len(),
        ) {
            match reason.as_str() {
                "already claimed" | "already running" => result.skipped_claimed += 1,
                "blocked" => result.skipped_blocked += 1,
                _ => {}
            }
            continue;
        }

        // Check concurrency limit (may have dispatched this tick)
        if state.running.len() >= config.max_concurrent {
            break;
        }

        // Local dispatch must establish Issue/config project ownership before
        // a provider Session is launched.  The post-launch check below still
        // verifies the newly-created Session, but it cannot be the first
        // project boundary: a mismatched tracker/config pair must have zero
        // launches and leave no claimed or dispatch state behind.
        if config.kind == "local" {
            let project_matches = match (config.project_id, Uuid::parse_str(&issue.id)) {
                (Some(project_id), Ok(issue_id)) => store
                    .lock()
                    .await
                    .get_issue_in_project(project_id, issue_id)?
                    .is_some(),
                _ => false,
            };
            if !project_matches {
                let message = format!(
                    "Local issue dispatch project mismatch before launch for {}",
                    issue.identifier
                );
                tracing::error!("{message}");
                event_bus.publish(DaemonEvent::SystemMessage {
                    level: "error".to_string(),
                    message: message.clone(),
                });
                result.errors.push(message);
                continue;
            }
        }

        // Build LaunchConfig for issue-driven session
        let query = format!(
            "[{}] {}\n\nIssue: {}\nURL: {}{}",
            issue.identifier,
            issue.title,
            issue.identifier,
            issue.url,
            issue
                .description
                .as_deref()
                .map(|d| format!("\n\nDescription:\n{}", d))
                .unwrap_or_default(),
        );

        let launch_config = LaunchConfig {
            query: query.clone(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            working_dir: Some(config.working_dir.clone()),
            provider: Some(config.provider),
            model: config.model.clone(),
            configured_context_window: None,
            max_turns: config.max_turns,
            system_prompt: None,
            resume_session_id: None,
            session_kind: None,
            project_id: config.project_id,
            rsi_session_id: None,
            rsi_socket: None,
            rsi_session_token: None,
            continued_from: None,
            openai_base_url: None,
            openai_api_key: None,
            conversation_history: None,
            workflow_id: None,
            workflow_id_override: None,
            max_retries: Some(config.max_retries),
            group_id: None,
            parent_id: None,
            effort: None,
            issue_identifier: Some(issue.identifier.clone()),
            issue_url: Some(issue.url.clone()),
            issue_tracker_id: Some(issue.id.clone()),
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: Some(format!(
                "issue_tracker.dispatch:{}:{}:{}",
                config.kind,
                issue.id,
                issue.updated_at.to_rfc3339()
            )),
            model_invocation_request_fingerprint: Some(hash_request_fingerprint(&[
                rsi_common::model_control::ModelInvocationPurpose::IssueTrackerDispatch.as_str(),
                &config.kind,
                &issue.id,
                &issue.identifier,
                issue.updated_at.to_rfc3339().as_str(),
                config.model.as_deref().unwrap_or(""),
                &query,
            ])),
            skip_project_model_default: false,
            model_invocation_purpose:
                rsi_common::model_control::ModelInvocationPurpose::IssueTrackerDispatch,
            sandbox: None,
            cargo_target_dir: None,
            execution_scratch: None,
            // RSI-006: production launches default to false on both flags.
            is_eval: false,
            skip_context_pipeline: false,
            // Issue-driven launches don't carry a command frontmatter. Leave
            // unset so the validator stays silent for tracker-dispatched work.
            capability_class: None,
            // Issue-tracker launches carry no tag set at spawn time.
            tags: vec![],
            // Issue-tracker launches are never topology-bound.
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
        };

        match session_launcher.launch(launch_config).await {
            Ok(session_id) => {
                if config.kind == "local" {
                    let project_matches = match config.project_id {
                        Some(project_id) => {
                            store.lock().await.local_issue_dispatch_matches_project(
                                &issue.id, session_id, project_id,
                            )?
                        }
                        None => false,
                    };
                    if !project_matches {
                        let message = format!(
                            "Local issue dispatch ownership mismatch for {} and session {}",
                            issue.identifier, session_id
                        );
                        tracing::error!("{message}");
                        event_bus.publish(DaemonEvent::SystemMessage {
                            level: "error".to_string(),
                            message: message.clone(),
                        });
                        result.errors.push(message);
                        continue;
                    }
                }
                let dispatch = DispatchRecord {
                    issue_id: issue.id.clone(),
                    issue_identifier: issue.identifier.clone(),
                    tracker: config.kind.clone(),
                    session_id,
                    dispatched_at: chrono::Utc::now(),
                    last_reconciled_at: None,
                    terminal_state: None,
                };

                // Persist dispatch record
                {
                    let store = store.lock().await;
                    if let Err(e) = store.insert_issue_dispatch(&dispatch) {
                        tracing::error!(error = %e, issue = %issue.identifier, "Failed to persist dispatch record");
                    }
                }

                state.claimed.insert(issue.id.clone());
                state.running.insert(issue.id.clone(), dispatch);
                result.dispatched += 1;

                event_bus.publish(DaemonEvent::IssueDispatched {
                    issue_id: issue.id.clone(),
                    issue_identifier: issue.identifier.clone(),
                    session_id,
                    tracker: config.kind.clone(),
                });

                tracing::info!(
                    issue = %issue.identifier,
                    session_id = %session_id,
                    "Dispatched issue-driven session"
                );
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    issue = %issue.identifier,
                    "Failed to launch session for issue"
                );
                result
                    .errors
                    .push(format!("launch {}: {}", issue.identifier, e));
            }
        }
    }

    // 5. Publish tick result event
    event_bus.publish(DaemonEvent::IssueTrackerPolled {
        issues_found: result.issues_found,
        dispatched: result.dispatched,
        skipped_claimed: result.skipped_claimed,
        skipped_blocked: result.skipped_blocked,
    });

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::super::types::IssueState;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock tracker that returns configurable candidates.
    struct MockTracker {
        candidates: Vec<TrackedIssue>,
        by_ids: Vec<TrackedIssue>,
    }

    #[async_trait]
    impl Tracker for MockTracker {
        async fn fetch_candidates(
            &self,
            _config: &IssueTrackerConfig,
        ) -> Result<Vec<TrackedIssue>> {
            Ok(self.candidates.clone())
        }
        async fn fetch_by_ids(
            &self,
            _config: &IssueTrackerConfig,
            _ids: &[String],
        ) -> Result<Vec<TrackedIssue>> {
            Ok(self.by_ids.clone())
        }
        async fn update_issue_state(
            &self,
            _config: &IssueTrackerConfig,
            _id: &str,
            _state: &str,
        ) -> Result<()> {
            Ok(())
        }
        async fn resolve_viewer_id(&self, _config: &IssueTrackerConfig) -> Result<String> {
            Ok("user-1".to_string())
        }
    }

    /// Mock launcher that counts launches.
    struct MockLauncher {
        count: AtomicUsize,
    }

    #[async_trait]
    impl SessionLauncher for MockLauncher {
        async fn launch(&self, _config: LaunchConfig) -> Result<Uuid> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(Uuid::new_v4())
        }
    }

    fn make_issue(id: &str, state_type: &str) -> TrackedIssue {
        TrackedIssue {
            id: id.to_string(),
            identifier: format!("ENG-{}", id),
            title: format!("Issue {}", id),
            description: None,
            priority: Some(3),
            state: IssueState {
                id: "state-1".to_string(),
                name: "In Progress".to_string(),
                state_type: state_type.to_string(),
            },
            branch_name: None,
            url: format!("https://linear.app/team/ENG-{}", id),
            labels: vec![],
            blocked_by: vec![],
            assignee_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn test_config(max_concurrent: usize) -> IssueTrackerConfig {
        IssueTrackerConfig {
            max_concurrent,
            active_states: vec!["started".to_string()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn tick_dispatches_up_to_max_concurrent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let store = Arc::new(Mutex::new(Store::open(&db_path).unwrap()));
        let event_bus = EventBus::new(16);

        let tracker = MockTracker {
            candidates: vec![
                make_issue("1", "started"),
                make_issue("2", "started"),
                make_issue("3", "started"),
            ],
            by_ids: vec![],
        };
        let launcher = MockLauncher {
            count: AtomicUsize::new(0),
        };
        let mut state = PollerState::new();
        let config = test_config(2);

        let result = tick(&tracker, &config, &mut state, &launcher, &store, &event_bus)
            .await
            .unwrap();

        assert_eq!(result.issues_found, 3);
        assert_eq!(result.dispatched, 2);
        assert_eq!(launcher.count.load(Ordering::SeqCst), 2);
        assert_eq!(state.running.len(), 2);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn local_dispatch_mismatch_is_rejected_before_launch_or_state_mutation() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let issue = store
            .lock()
            .await
            .create_issue(&rsi_common::types::NewIssue {
                project_id: crate::store::d04_test_project_id(),
                title: "wrong local project".to_string(),
                body: String::new(),
                priority: None,
                labels: Vec::new(),
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap();
        let mut candidate = make_issue(&issue.id.to_string(), "started");
        candidate.identifier = "LOCAL-mismatch".to_string();
        let tracker = MockTracker {
            candidates: vec![candidate],
            by_ids: Vec::new(),
        };
        let launcher = MockLauncher {
            count: AtomicUsize::new(0),
        };
        let config = IssueTrackerConfig {
            kind: "local".to_string(),
            project_id: Some(Uuid::new_v4()),
            ..test_config(1)
        };
        let event_bus = EventBus::new(16);
        let mut state = PollerState::new();
        let result = tick(&tracker, &config, &mut state, &launcher, &store, &event_bus)
            .await
            .unwrap();
        assert_eq!(launcher.count.load(Ordering::SeqCst), 0);
        assert_eq!(result.dispatched, 0);
        assert_eq!(state.running.len(), 0);
        assert!(
            store
                .lock()
                .await
                .load_dispatch_by_issue_id(&issue.id.to_string())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn tick_reconciles_terminal_issues() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let store = Arc::new(Mutex::new(Store::open(&db_path).unwrap()));
        let event_bus = EventBus::new(16);

        // Pre-populate running state
        let mut state = PollerState::new();
        let dispatch = DispatchRecord {
            issue_id: "1".to_string(),
            issue_identifier: "ENG-1".to_string(),
            tracker: "linear".to_string(),
            session_id: Uuid::new_v4(),
            dispatched_at: chrono::Utc::now(),
            last_reconciled_at: None,
            terminal_state: None,
        };
        state.running.insert("1".to_string(), dispatch.clone());

        // Store dispatch record
        {
            let s = store.lock().await;
            let _ = s.insert_issue_dispatch(&dispatch);
        }

        // Tracker returns terminal state for the running issue
        let tracker = MockTracker {
            candidates: vec![],
            by_ids: vec![make_issue("1", "completed")],
        };
        let launcher = MockLauncher {
            count: AtomicUsize::new(0),
        };
        let config = test_config(5);

        let result = tick(&tracker, &config, &mut state, &launcher, &store, &event_bus)
            .await
            .unwrap();

        assert_eq!(
            state.running.len(),
            0,
            "Terminal issue should be removed from running"
        );
        assert_eq!(result.dispatched, 0);
    }

    #[tokio::test]
    async fn tick_with_zero_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let store = Arc::new(Mutex::new(Store::open(&db_path).unwrap()));
        let event_bus = EventBus::new(16);

        let tracker = MockTracker {
            candidates: vec![],
            by_ids: vec![],
        };
        let launcher = MockLauncher {
            count: AtomicUsize::new(0),
        };
        let mut state = PollerState::new();
        let config = test_config(5);

        let result = tick(&tracker, &config, &mut state, &launcher, &store, &event_bus)
            .await
            .unwrap();

        assert_eq!(result.issues_found, 0);
        assert_eq!(result.dispatched, 0);
        assert_eq!(launcher.count.load(Ordering::SeqCst), 0);
    }
}
