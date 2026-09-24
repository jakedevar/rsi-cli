//! Native single-user issue tracker backed directly by the daemon's own
//! store (C1 `issues`/`issue_deps`, `crates/rsid/src/store/issues.rs`).
//!
//! Selected in place of Linear via `RSI_ISSUE_TRACKER_KIND=local`
//! (`config.rs::issue_tracker_config`); see
//! `thoughts/shared/plans/2026-07-13-C2-local-tracker.md` (slice C2) for the
//! design record.

use super::tracker::Tracker;
use super::types::{IssueState, IssueTrackerConfig, TrackedIssue};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use async_trait::async_trait;
use rsi_common::types::{Issue, IssueStatus};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

/// `Tracker` impl over the local single-user store.
///
/// **D-C2-3 (binding):** `LocalTracker` IGNORES `IssueTrackerConfig::assignee`
/// on every method — the local store has no multi-user assignee concept (a
/// single operator owns the whole store), so candidates are never filtered by
/// assignee the way `LinearClient::fetch_candidates` filters by
/// `config.assignee` (`linear.rs:311`).
///
/// **Lock discipline (binding):** each method locks `store` transiently and
/// drops the guard before returning; the guard is never held across an await
/// that leaves the method. The poller's own store locks
/// (`poller.rs:130`,`:247`) run strictly after `fetch_*`/`update_issue_state`
/// return, so there is no nesting and no deadlock risk.
pub struct LocalTracker {
    store: Arc<Mutex<Store>>,
    project_id: Uuid,
}

impl LocalTracker {
    pub const fn new_for_project(store: Arc<Mutex<Store>>, project_id: Uuid) -> Self {
        Self { store, project_id }
    }

    #[cfg(test)]
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self::new_for_project(store, crate::store::d04_test_project_id())
    }

    /// The local tracker is construction-bound to a single project.  A
    /// separately assembled tracker/config pair is an integrity boundary, not
    /// a best-effort hint: reject drift before consulting the Store so a
    /// project-A tracker can never select, reconcile, or close work under a
    /// project-B launch configuration.
    fn require_matching_config_project(&self, config: &IssueTrackerConfig) -> Result<Uuid> {
        if config.kind != "local" {
            return Err(DaemonError::Store(format!(
                "LocalTracker requires local configuration, got {}",
                config.kind
            )));
        }
        let configured_project = config.project_id.ok_or_else(|| {
            DaemonError::Store("LocalTracker configuration is missing project binding".to_string())
        })?;
        if configured_project != self.project_id {
            return Err(DaemonError::Store(
                "LocalTracker configuration project does not match tracker binding".to_string(),
            ));
        }
        Ok(configured_project)
    }

    /// Normalize a store `Issue` into a `TrackedIssue`.
    ///
    /// **D-C2-2 (binding):** `identifier = LOCAL-{display_number}`,
    /// `url = rsi://issue/{display_number}` — both synthetic, stable, and
    /// unique (fed straight from the store's monotonic `display_number`).
    ///
    /// Status forward-mapping (both directions documented on
    /// `update_issue_state` below): `Open` -> `"unstarted"`,
    /// `InProgress` -> `"started"` (both in the default `active_states`),
    /// `Closed` -> `"completed"`, `Cancelled` -> `"cancelled"` (terminal —
    /// lets the poller's reconciliation pass at `poller.rs:110` detect local
    /// closure via `fetch_by_ids`).
    ///
    /// `blocked_by` is always empty: `list_ready_issues` (the only source of
    /// `fetch_candidates` results) already excludes anything with an open
    /// blocker, so the eligibility blocker gate (`eligibility.rs:49`) is
    /// trivially satisfied on an empty vec regardless of caller.
    fn normalize(issue: &Issue) -> TrackedIssue {
        let state_type = match issue.status {
            IssueStatus::Open => "unstarted",
            IssueStatus::InProgress => "started",
            IssueStatus::Closed => "completed",
            IssueStatus::Cancelled => "cancelled",
        };

        TrackedIssue {
            id: issue.id.to_string(),
            identifier: format!("LOCAL-{}", issue.display_number),
            title: issue.title.clone(),
            description: (!issue.body.is_empty()).then(|| issue.body.clone()),
            priority: issue.priority,
            state: IssueState {
                id: state_type.to_string(),
                name: issue.status.as_str().to_string(),
                state_type: state_type.to_string(),
            },
            branch_name: None,
            url: format!("rsi://issue/{}", issue.display_number),
            labels: issue.labels.clone(),
            blocked_by: Vec::new(),
            assignee_id: issue.assignee.clone(),
            created_at: issue.created_at,
            updated_at: issue.updated_at,
        }
    }
}

#[async_trait]
impl Tracker for LocalTracker {
    /// Ready-work candidates: `store.list_ready_issues(None)` (Open, no open
    /// blocker, priority-then-age) mapped through `normalize`. Ignores
    /// `config.assignee` (D-C2-3).
    async fn fetch_candidates(&self, config: &IssueTrackerConfig) -> Result<Vec<TrackedIssue>> {
        let project_id = self.require_matching_config_project(config)?;
        let issues = {
            let store = self.store.lock().await;
            store.list_ready_issues(Some(project_id), None)?
        };
        Ok(issues.iter().map(Self::normalize).collect())
    }

    /// Reconciliation lookup: parse each id as a `Uuid` and `get_issue` it.
    /// An id that fails to parse, or that parses but is not found in the
    /// store, is silently omitted from the result (never an error) — this
    /// matches the poller's reconciliation loop (`poller.rs:104`), which only
    /// cares about the ids it *does* get back.
    ///
    /// F4 (Track C slice C3, review obligation carried from C2): a genuine
    /// store error (as opposed to a clean "not found") propagates as `Err`
    /// rather than being warn+omitted — matching `LinearClient::fetch_by_ids`
    /// parity. The poller's `tick` already treats a `fetch_by_ids` `Err` as a
    /// tick error (logged + `SystemMessage`, retried next interval); an
    /// affected id simply stays `running`/claimed and is retried, it is never
    /// silently dropped from state.
    async fn fetch_by_ids(
        &self,
        config: &IssueTrackerConfig,
        ids: &[String],
    ) -> Result<Vec<TrackedIssue>> {
        let project_id = self.require_matching_config_project(config)?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let store = self.store.lock().await;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            match Uuid::parse_str(id) {
                Ok(uuid) => match store.get_issue_in_project(project_id, uuid) {
                    Ok(Some(issue)) => out.push(Self::normalize(&issue)),
                    Ok(None) => {
                        tracing::debug!(id = %id, "LocalTracker::fetch_by_ids: unknown issue id, omitting");
                    }
                    Err(e) => {
                        tracing::warn!(id = %id, error = %e, "LocalTracker::fetch_by_ids: store error, propagating");
                        return Err(e);
                    }
                },
                Err(e) => {
                    tracing::warn!(id = %id, error = %e, "LocalTracker::fetch_by_ids: invalid uuid, omitting");
                }
            }
        }
        drop(store);
        Ok(out)
    }

    /// Post-completion state transition (only invoked by the manager when
    /// `config.completion_state` is set — unset means the manager
    /// early-returns before ever calling this, per D-C2-4, so "no
    /// auto-close by default" is enforced upstream, not here).
    ///
    /// **D-C2-4 (binding) state-name mapping:** `"completed"` / `"closed"` ->
    /// `IssueStatus::Closed`; `"cancelled"` -> `IssueStatus::Cancelled`; any
    /// other (unknown) `state_name` -> `tracing::warn!` + `IssueStatus::Closed`
    /// (never panics, never silently no-ops). A `issue_id` that fails to parse
    /// as a `Uuid` is a hard `Err`, not a panic.
    async fn update_issue_state(
        &self,
        config: &IssueTrackerConfig,
        issue_id: &str,
        state_name: &str,
    ) -> Result<()> {
        let project_id = self.require_matching_config_project(config)?;
        let uuid = Uuid::parse_str(issue_id).map_err(|e| {
            DaemonError::Store(format!(
                "LocalTracker::update_issue_state: invalid issue id '{issue_id}': {e}"
            ))
        })?;

        // Case-insensitive per plan P-002: accepts the lowercase Linear-style
        // names AND the canonical serde variant strings (`Closed`,
        // `Cancelled`) — enum strings elsewhere match serde variants exactly,
        // so a completion_state configured with the canonical form must not
        // fall through to the unknown-name arm (review F2).
        let status = match state_name.to_ascii_lowercase().as_str() {
            "completed" | "closed" => IssueStatus::Closed,
            "cancelled" => IssueStatus::Cancelled,
            _ => {
                tracing::warn!(
                    state_name = %state_name,
                    issue_id = %issue_id,
                    "LocalTracker::update_issue_state: unknown state name, defaulting to Closed"
                );
                IssueStatus::Closed
            }
        };

        let store = self.store.lock().await;
        store.update_issue_status_in_project(project_id, uuid, status)?;
        drop(store);
        Ok(())
    }

    /// No API viewer for a local single-user store; the constant `"local"`
    /// is the whole answer (assignee filtering is a no-op regardless, per
    /// D-C2-3).
    async fn resolve_viewer_id(&self, _config: &IssueTrackerConfig) -> Result<String> {
        Ok("local".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::claude::LaunchConfig;
    use crate::issue_tracker::poller::{PollerState, SessionLauncher, tick};
    use rsi_common::types::NewIssue;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_new_issue(title: &str) -> NewIssue {
        NewIssue {
            project_id: crate::store::d04_test_project_id(),
            title: title.to_string(),
            body: String::new(),
            priority: None,
            labels: Vec::new(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        }
    }

    fn make_store() -> Arc<Mutex<Store>> {
        Arc::new(Mutex::new(Store::open_in_memory().unwrap()))
    }

    fn local_config() -> IssueTrackerConfig {
        IssueTrackerConfig {
            kind: "local".to_string(),
            project_id: Some(crate::store::d04_test_project_id()),
            ..IssueTrackerConfig::default()
        }
    }

    #[tokio::test]
    async fn resolve_viewer_id_is_local() {
        let tracker = LocalTracker::new(make_store());
        let config = local_config();
        assert_eq!(tracker.resolve_viewer_id(&config).await.unwrap(), "local");
    }

    #[tokio::test]
    async fn fetch_candidates_maps_ready_issues_and_excludes_blocked() {
        let store = make_store();
        let (a, b) = {
            let s = store.lock().await;
            let a = s
                .create_issue(&NewIssue {
                    priority: Some(2),
                    ..make_new_issue("a")
                })
                .unwrap();
            let b = s.create_issue(&make_new_issue("b")).unwrap();
            let blocked = s.create_issue(&make_new_issue("blocked")).unwrap();
            // `blocked` depends on (is blocked by) `a`, which is still Open,
            // so `list_ready_issues` must exclude it.
            s.add_issue_dep(blocked.id, a.id).unwrap();
            (a, b)
        };

        let tracker = LocalTracker::new(Arc::clone(&store));
        let config = local_config();
        let candidates = tracker.fetch_candidates(&config).await.unwrap();

        assert_eq!(candidates.len(), 2, "the blocked issue must be excluded");

        let find = |id: Uuid| {
            candidates
                .iter()
                .find(|c| c.id == id.to_string())
                .unwrap_or_else(|| panic!("issue {id} missing from candidates"))
        };

        let a_tracked = find(a.id);
        assert_eq!(a_tracked.identifier, format!("LOCAL-{}", a.display_number));
        assert_eq!(a_tracked.url, format!("rsi://issue/{}", a.display_number));
        assert_eq!(a_tracked.priority, Some(2));
        assert_eq!(a_tracked.state.state_type, "unstarted");
        assert!(a_tracked.blocked_by.is_empty());

        let b_tracked = find(b.id);
        assert_eq!(b_tracked.identifier, format!("LOCAL-{}", b.display_number));
        assert_eq!(b_tracked.state.state_type, "unstarted");
        assert!(b_tracked.blocked_by.is_empty());
    }

    #[tokio::test]
    async fn fetch_by_ids_maps_every_status_and_skips_unknown_or_bad_ids() {
        let store = make_store();
        let (open_issue, in_progress_issue, closed_issue, cancelled_issue) = {
            let s = store.lock().await;
            let open = s.create_issue(&make_new_issue("open")).unwrap();
            let in_progress = s.create_issue(&make_new_issue("in-progress")).unwrap();
            s.update_issue_status(in_progress.id, IssueStatus::InProgress)
                .unwrap();
            let closed = s.create_issue(&make_new_issue("closed")).unwrap();
            s.update_issue_status(closed.id, IssueStatus::Closed)
                .unwrap();
            let cancelled = s.create_issue(&make_new_issue("cancelled")).unwrap();
            s.update_issue_status(cancelled.id, IssueStatus::Cancelled)
                .unwrap();
            (open, in_progress, closed, cancelled)
        };

        let tracker = LocalTracker::new(Arc::clone(&store));
        let config = local_config();

        let unknown_id = Uuid::new_v4().to_string();
        let ids = vec![
            open_issue.id.to_string(),
            in_progress_issue.id.to_string(),
            closed_issue.id.to_string(),
            cancelled_issue.id.to_string(),
            unknown_id,
            "not-a-uuid".to_string(),
        ];

        let fetched = tracker.fetch_by_ids(&config, &ids).await.unwrap();
        // Unknown id and the unparseable id are both silently omitted, no panic.
        assert_eq!(fetched.len(), 4);

        let state_type_of = |id: Uuid| -> String {
            fetched
                .iter()
                .find(|t| t.id == id.to_string())
                .unwrap_or_else(|| panic!("issue {id} missing from fetch_by_ids result"))
                .state
                .state_type
                .clone()
        };

        assert_eq!(state_type_of(open_issue.id), "unstarted");
        assert_eq!(state_type_of(in_progress_issue.id), "started");
        assert_eq!(state_type_of(closed_issue.id), "completed");
        assert_eq!(state_type_of(cancelled_issue.id), "cancelled");
    }

    #[tokio::test]
    async fn fetch_by_ids_empty_input_short_circuits() {
        let tracker = LocalTracker::new(make_store());
        let config = local_config();
        assert!(tracker.fetch_by_ids(&config, &[]).await.unwrap().is_empty());
    }

    /// F4: a genuine store error (not a clean "not found") must propagate as
    /// `Err`, not be silently warn+omitted like an unknown id. Trigger a
    /// store error the cheapest reliable way: drop the V97 Issue/event
    /// relations out from under a live, already-created issue id (with FKs
    /// deliberately disabled only in this corruption fixture), so
    /// `get_issue`'s `SELECT` fails at the SQLite layer.
    #[tokio::test]
    async fn fetch_by_ids_propagates_store_errors_instead_of_silently_omitting() {
        let store = make_store();
        let issue = {
            let s = store.lock().await;
            s.create_issue(&make_new_issue("will-error")).unwrap()
        };
        {
            let s = store.lock().await;
            s.conn
                .execute_batch(
                    "PRAGMA foreign_keys=OFF;
                     DROP TABLE issue_events;
                     DROP TABLE issues;
                     PRAGMA foreign_keys=ON;",
                )
                .unwrap();
        }

        let tracker = LocalTracker::new(Arc::clone(&store));
        let config = local_config();
        let result = tracker.fetch_by_ids(&config, &[issue.id.to_string()]).await;
        assert!(
            result.is_err(),
            "a store error must propagate as Err, not a silent Ok"
        );
    }

    #[tokio::test]
    async fn issue_writer_update_issue_state_completed_is_project_scoped_and_audited() {
        let store = make_store();
        let issue = {
            let s = store.lock().await;
            s.create_issue(&make_new_issue("to-close")).unwrap()
        };
        let tracker = LocalTracker::new(Arc::clone(&store));
        let config = local_config();

        tracker
            .update_issue_state(&config, &issue.id.to_string(), "completed")
            .await
            .unwrap();

        let (reloaded, history) = {
            let s = store.lock().await;
            (
                s.get_issue(issue.id).unwrap().unwrap(),
                s.list_issue_events_v1(&rsi_common::types::IssueEventPageRequestV1 {
                    issue_id: issue.id,
                    after_sequence: 0,
                    limit: None,
                })
                .unwrap(),
            )
        };
        assert_eq!(reloaded.status, IssueStatus::Closed);
        assert!(reloaded.closed_at.is_some());
        assert_eq!(Some(reloaded.project_id), config.project_id);
        assert_eq!(history.events.len(), 2);
        let event = history.events.last().unwrap();
        assert_eq!(
            event.actor_kind,
            rsi_common::types::IssueActorKindV1::System
        );
        assert_eq!(
            event.actor_label.as_deref(),
            Some("rsi:local-issue-tracker")
        );
        assert_eq!(event.issue, reloaded);
        assert_eq!(
            event.request.fingerprint().unwrap(),
            event.request_fingerprint
        );
        assert!(matches!(
            event.request.operation,
            rsi_common::types::IssueSemanticOperationV1::StatusUpdated {
                issue_id,
                expected_row_version,
                status: IssueStatus::Closed,
            } if issue_id == issue.id && expected_row_version == issue.row_version
        ));
    }

    #[tokio::test]
    async fn update_issue_state_cancelled_maps_to_cancelled() {
        let store = make_store();
        let issue = {
            let s = store.lock().await;
            s.create_issue(&make_new_issue("to-cancel")).unwrap()
        };
        let tracker = LocalTracker::new(Arc::clone(&store));
        let config = local_config();

        tracker
            .update_issue_state(&config, &issue.id.to_string(), "cancelled")
            .await
            .unwrap();

        let reloaded = {
            let s = store.lock().await;
            s.get_issue(issue.id).unwrap().unwrap()
        };
        assert_eq!(reloaded.status, IssueStatus::Cancelled);
        assert!(reloaded.closed_at.is_some());
    }

    #[tokio::test]
    async fn update_issue_state_unknown_name_warns_and_defaults_to_closed() {
        let store = make_store();
        let issue = {
            let s = store.lock().await;
            s.create_issue(&make_new_issue("weird-state")).unwrap()
        };
        let tracker = LocalTracker::new(Arc::clone(&store));
        let config = local_config();

        tracker
            .update_issue_state(&config, &issue.id.to_string(), "some-unknown-state")
            .await
            .unwrap();

        let reloaded = {
            let s = store.lock().await;
            s.get_issue(issue.id).unwrap().unwrap()
        };
        assert_eq!(reloaded.status, IssueStatus::Closed);
    }

    #[tokio::test]
    async fn update_issue_state_bad_uuid_errors_without_panic() {
        let tracker = LocalTracker::new(make_store());
        let config = local_config();

        let result = tracker
            .update_issue_state(&config, "not-a-uuid", "completed")
            .await;
        assert!(result.is_err());
    }

    /// Mock launcher mirroring `poller.rs:335`'s test-only `MockLauncher` —
    /// counts launches instead of spawning real sessions.
    struct MockLauncher {
        count: AtomicUsize,
        store: Arc<Mutex<Store>>,
    }

    #[async_trait]
    impl SessionLauncher for MockLauncher {
        async fn launch(&self, config: LaunchConfig) -> Result<Uuid> {
            self.count.fetch_add(1, Ordering::SeqCst);
            let mut session = crate::store::tests::make_test_session();
            session.id = Uuid::new_v4();
            session.project_id = config.project_id;
            self.store.lock().await.insert_session(&session)?;
            Ok(session.id)
        }
    }

    /// Trait-level poller test: `LocalTracker` behind `&dyn Tracker`, driven
    /// through the real `poller::tick` pipeline against a seeded in-memory
    /// store — mirrors `tick_dispatches_up_to_max_concurrent`
    /// (`poller.rs:378`), but through the local store path instead of a
    /// `MockTracker`.
    #[tokio::test]
    async fn poller_tick_dispatches_ready_local_issues_bounded_by_max_concurrent() {
        let store = make_store();
        {
            let s = store.lock().await;
            for i in 0..3 {
                s.create_issue(&make_new_issue(&format!("issue-{i}")))
                    .unwrap();
            }
        }

        let tracker = LocalTracker::new(Arc::clone(&store));
        let launcher = MockLauncher {
            count: AtomicUsize::new(0),
            store: Arc::clone(&store),
        };
        let event_bus = EventBus::new(16);
        let mut state = PollerState::new();
        let config = IssueTrackerConfig {
            kind: "local".to_string(),
            project_id: Some(crate::store::d04_test_project_id()),
            max_concurrent: 2,
            ..IssueTrackerConfig::default()
        };

        let result = tick(&tracker, &config, &mut state, &launcher, &store, &event_bus)
            .await
            .unwrap();

        assert_eq!(result.issues_found, 3);
        assert_eq!(result.dispatched, 2);
        assert_eq!(launcher.count.load(Ordering::SeqCst), 2);
        assert_eq!(state.running.len(), 2);
    }

    /// Review F2/F5c: the canonical serde variant strings (`Closed`,
    /// `Cancelled`) must map like their lowercase Linear-style aliases —
    /// never fall through to the unknown-name arm.
    #[tokio::test]
    async fn update_issue_state_accepts_canonical_capitalized_names() {
        let store = make_store();
        let (a, b) = {
            let s = store.lock().await;
            (
                s.create_issue(&make_new_issue("cap-closed")).unwrap(),
                s.create_issue(&make_new_issue("cap-cancelled")).unwrap(),
            )
        };
        let tracker = LocalTracker::new(Arc::clone(&store));
        let config = local_config();

        tracker
            .update_issue_state(&config, &a.id.to_string(), "Closed")
            .await
            .unwrap();
        tracker
            .update_issue_state(&config, &b.id.to_string(), "Cancelled")
            .await
            .unwrap();

        let s = store.lock().await;
        assert_eq!(
            s.get_issue(a.id).unwrap().unwrap().status,
            IssueStatus::Closed
        );
        assert_eq!(
            s.get_issue(b.id).unwrap().unwrap().status,
            IssueStatus::Cancelled
        );
    }

    /// Review F5d: empty body maps to `description: None`, non-empty body to
    /// `Some(body)`.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn fetch_candidates_maps_empty_body_to_none_description() {
        let store = make_store();
        let (bare, described) = {
            let s = store.lock().await;
            let bare = s.create_issue(&make_new_issue("no-body")).unwrap();
            let described = s
                .create_issue(&NewIssue {
                    body: "the body".to_string(),
                    ..make_new_issue("with-body")
                })
                .unwrap();
            (bare, described)
        };
        let tracker = LocalTracker::new(Arc::clone(&store));
        let candidates = tracker.fetch_candidates(&local_config()).await.unwrap();

        let find = |id: Uuid| candidates.iter().find(|c| c.id == id.to_string()).unwrap();
        assert_eq!(find(bare.id).description, None);
        assert_eq!(find(described.id).description, Some("the body".to_string()));
    }

    /// Review F5a: the local-specific composition — a dispatched issue stays
    /// `Open` in the store (C2 has no in-band close), so it reappears as a
    /// candidate on every tick; the poller's claimed-set must skip it rather
    /// than re-dispatch. `max_concurrent` is deliberately larger than the
    /// issue count so a dedup failure would be visible as extra launches
    /// instead of being masked by slot exhaustion.
    #[tokio::test]
    async fn poller_second_tick_does_not_redispatch_claimed_open_issues() {
        let store = make_store();
        {
            let s = store.lock().await;
            for i in 0..3 {
                s.create_issue(&make_new_issue(&format!("sticky-{i}")))
                    .unwrap();
            }
        }

        let tracker = LocalTracker::new(Arc::clone(&store));
        let launcher = MockLauncher {
            count: AtomicUsize::new(0),
            store: Arc::clone(&store),
        };
        let event_bus = EventBus::new(16);
        let mut state = PollerState::new();
        let config = IssueTrackerConfig {
            kind: "local".to_string(),
            project_id: Some(crate::store::d04_test_project_id()),
            max_concurrent: 5,
            ..IssueTrackerConfig::default()
        };

        let first = tick(&tracker, &config, &mut state, &launcher, &store, &event_bus)
            .await
            .unwrap();
        assert_eq!(first.issues_found, 3);
        assert_eq!(first.dispatched, 3);

        let second = tick(&tracker, &config, &mut state, &launcher, &store, &event_bus)
            .await
            .unwrap();
        // All three are still Open (and thus re-listed) but claimed.
        assert_eq!(second.issues_found, 3);
        assert_eq!(second.dispatched, 0, "claimed issues must not re-dispatch");
        assert_eq!(launcher.count.load(Ordering::SeqCst), 3);
        assert_eq!(state.running.len(), 3);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn config_project_drift_rejects_candidates_by_id_completion_and_tick_before_store_or_launch()
     {
        let store = make_store();
        let issue = {
            let store = store.lock().await;
            store
                .create_issue(&make_new_issue("bound project only"))
                .unwrap()
        };
        let tracker = LocalTracker::new(Arc::clone(&store));
        let mut drifted = local_config();
        drifted.project_id = Some(Uuid::new_v4());

        assert!(tracker.fetch_candidates(&drifted).await.is_err());
        assert!(
            tracker
                .fetch_by_ids(&drifted, &[issue.id.to_string()])
                .await
                .is_err()
        );
        assert!(
            tracker
                .update_issue_state(&drifted, &issue.id.to_string(), "completed")
                .await
                .is_err()
        );

        let launcher = MockLauncher {
            count: AtomicUsize::new(0),
            store: Arc::clone(&store),
        };
        let mut state = PollerState::new();
        let result = tick(
            &tracker,
            &drifted,
            &mut state,
            &launcher,
            &store,
            &EventBus::new(16),
        )
        .await
        .unwrap();
        assert_eq!(launcher.count.load(Ordering::SeqCst), 0);
        assert!(state.claimed.is_empty() && state.running.is_empty());
        assert_eq!(result.dispatched, 0);
        let (status, dispatch) = {
            let store = store.lock().await;
            (
                store.get_issue(issue.id).unwrap().unwrap().status,
                store
                    .load_dispatch_by_issue_id(&issue.id.to_string())
                    .unwrap(),
            )
        };
        assert_eq!(status, IssueStatus::Open);
        assert!(dispatch.is_none());
    }
}
