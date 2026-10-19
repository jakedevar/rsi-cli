//! #2246: a codegraph that stays Degraded or Failed for over 24 hours is
//! reported, not retried silently. The sweep turns each project's
//! [`CodegraphHealthNoticeV1`] into one deduplicated Issue
//! (`Store::upsert_codegraph_health_issue`); the same notices ride
//! `GetHealthStatus` to the TUI health row.
//!
//! #2454: the day-old Issue is a project backstop for primary graphs. The
//! episode sweep ([`notify_failure_episodes`]) tells the workspace owner (or the
//! project manager) once per failure episode as soon as it starts, for any
//! checkout, so a sandbox failure or a failure after the Issue was closed is
//! not silent.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rsi_common::codegraph::{CODEGRAPH_UNHEALTHY_NOTICE_SECS, CodegraphHealthNoticeV1};

use super::IndexHandle;
use crate::store::Store;
use crate::store::codegraph_failure_notice::{CodegraphFailureReport, CodegraphFailureRoute};
use crate::store::codegraph_health::CodegraphHealthIssueAction;
use uuid::Uuid;

/// How often the sweep looks for graphs past the threshold (the first pass at boot).
const SWEEP_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// How often the sweep looks for a new failure episode (#2454). The scan is an
/// in-memory read; only an unseen episode reaches the store.
const EPISODE_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The failure's identity for dedupe: phase and error class, with byte counts
/// dropped so a write-budget failure compares equal run to run.
fn signature(notice: &CodegraphHealthNoticeV1) -> String {
    let error = notice.last_error.as_deref().unwrap_or("none");
    let error = if error.starts_with("codegraph physical write budget exhausted") {
        "codegraph physical write budget exhausted"
    } else {
        error
    };
    format!("{}: {error}", notice.phase)
}

/// The Issue title and body for one project's notice.
fn issue_text(
    notice: &CodegraphHealthNoticeV1,
    now: chrono::DateTime<chrono::Utc>,
) -> (String, String) {
    let state = if notice.phase == "degraded" {
        "degraded (stale head, newest run failing)"
    } else {
        "failed (no Ready head)"
    };
    let title = format!("kaizen: codegraph {state} for over 24 hours");
    let body = format!(
        "## Codegraph health (auto-filed by the daemon, #2246)\n\
         - project: `{project}`\n\
         - workspace: `{workspace}`\n\
         - state: {state}\n\
         - unhealthy since: {since}\n\
         - failed runs since then: {runs}\n\
         - last error: `{error}`\n\
         - last refreshed: {now}\n\n\
         ## Intent\n\
         Find why this project's graph cannot publish a Ready head and fix the cause, or \
         record why it is expected. The daemon retries an unchanged failure on a growing \
         delay (up to 6 hours), so a fix is picked up without a restart; toggling \
         `codegraph_indexing_enabled` forces an immediate retry. Live state: \
         `GetCodegraphStatus` and the TUI health row. The daemon refreshes this Issue when \
         the error changes and files no second one for the project; close it to \
         acknowledge.\n",
        project = notice.project_id,
        workspace = notice.workspace_id,
        since = notice.unhealthy_since,
        runs = notice.failed_runs,
        error = notice.last_error.as_deref().unwrap_or("none recorded"),
        now = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    (title, body)
}

/// File or refresh one Issue per notice. A failure for one project never
/// stops the others; the count of Issues written is returned.
pub fn file_notices(
    store: &Store,
    notices: &[CodegraphHealthNoticeV1],
    now: chrono::DateTime<chrono::Utc>,
) -> usize {
    let mut written = 0;
    for notice in notices {
        let (title, body) = issue_text(notice, now);
        match store.upsert_codegraph_health_issue(
            notice.project_id,
            &signature(notice),
            &title,
            &body,
            now,
        ) {
            Ok(issue) => {
                if matches!(
                    issue.action,
                    CodegraphHealthIssueAction::Created | CodegraphHealthIssueAction::Refreshed
                ) {
                    written += 1;
                    tracing::warn!(
                        project_id = %notice.project_id,
                        issue = issue.display_number,
                        action = ?issue.action,
                        "codegraph unhealthy for over 24 hours; Issue filed or refreshed"
                    );
                }
            }
            Err(error) => tracing::warn!(
                project_id = %notice.project_id,
                %error,
                "codegraph health Issue deferred"
            ),
        }
    }
    written
}

/// Deliver one owner notice per unseen failure episode. `notified` holds the
/// `(workspace, episode)` pairs already handled this process and is pruned to
/// the episodes still current, so it stays bounded by the registered
/// workspaces. A store error leaves the episode unmarked for the next sweep;
/// the store dedupes by episode, so a repeat never sends twice. Returns the
/// notices newly written.
pub fn notify_failure_episodes(
    store: &Store,
    episodes: &[CodegraphFailureReport],
    notified: &mut HashSet<(Uuid, String)>,
) -> usize {
    let current = episodes
        .iter()
        .map(|episode| (episode.workspace_id, episode.episode_key()))
        .collect::<HashSet<_>>();
    notified.retain(|key| current.contains(key));
    let mut written = 0;
    for episode in episodes {
        let key = (episode.workspace_id, episode.episode_key());
        if notified.contains(&key) {
            continue;
        }
        match store.record_codegraph_failure_notice(episode) {
            Ok(route) => {
                notified.insert(key);
                if matches!(
                    route,
                    CodegraphFailureRoute::OwnerMail(_) | CodegraphFailureRoute::ManagerNotice(_)
                ) {
                    written += 1;
                    tracing::warn!(
                        project_id = %episode.project_id,
                        workspace_id = %episode.workspace_id,
                        checkout = %episode.checkout,
                        ?route,
                        "codegraph failure episode started; owner notified (#2454)"
                    );
                }
            }
            Err(error) => tracing::warn!(
                workspace_id = %episode.workspace_id,
                %error,
                "codegraph failure notice deferred"
            ),
        }
    }
    written
}

/// Sweep for new failure episodes every minute (#2454) and, every thirty
/// minutes, file the Issues for graphs past [`CODEGRAPH_UNHEALTHY_NOTICE_SECS`].
pub async fn run_codegraph_health_loop(handle: IndexHandle, store: Arc<tokio::sync::Mutex<Store>>) {
    debug_assert!(CODEGRAPH_UNHEALTHY_NOTICE_SECS > 0);
    let mut issues = tokio::time::interval(SWEEP_INTERVAL);
    issues.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut episodes = tokio::time::interval(EPISODE_SWEEP_INTERVAL);
    episodes.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut notified = HashSet::new();
    loop {
        tokio::select! {
            _ = episodes.tick() => {
                let current = handle.failure_episodes();
                if current.is_empty() && notified.is_empty() {
                    continue;
                }
                let store = store.lock().await;
                notify_failure_episodes(&store, &current, &mut notified);
            }
            _ = issues.tick() => {
                let now = chrono::Utc::now();
                let notices = handle.unhealthy_notices(now);
                if notices.is_empty() {
                    continue;
                }
                let store = store.lock().await;
                file_notices(&store, &notices, now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};
    use uuid::Uuid;

    fn notice(project_id: Uuid, error: &str) -> CodegraphHealthNoticeV1 {
        CodegraphHealthNoticeV1 {
            project_id,
            workspace_id: Uuid::new_v4(),
            phase: "failed".into(),
            unhealthy_since: "2026-09-25T00:00:00.000000000Z".into(),
            last_error: Some(error.into()),
            failed_runs: 1590,
        }
    }

    fn project(store: &Store) -> Uuid {
        let now = Utc::now();
        let project = rsi_common::types::Project {
            id: Uuid::new_v4(),
            name: format!("cg-{}", Uuid::new_v4()),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        store.insert_project(&project).unwrap();
        project.id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn write_budget_byte_counts_do_not_change_the_signature() {
        let mut a = notice(
            Uuid::new_v4(),
            "codegraph physical write budget exhausted (10 of 20 bytes)",
        );
        let mut b = a.clone();
        b.last_error = Some("codegraph physical write budget exhausted (11 of 20 bytes)".into());
        assert_eq!(signature(&a), signature(&b));
        a.last_error = Some("source path is a symlink".into());
        assert_ne!(signature(&a), signature(&b));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn sweep_files_one_issue_per_project_naming_the_error_2246() {
        let store = Store::open_in_memory().unwrap();
        let project_id = project(&store);
        let notices = [notice(project_id, "source path is a symlink")];
        let now = Utc::now();
        assert_eq!(file_notices(&store, &notices, now), 1);
        // The next sweeps, same error: nothing new is written.
        assert_eq!(
            file_notices(&store, &notices, now + ChronoDuration::minutes(30)),
            0
        );
        let id = crate::store::codegraph_health::codegraph_health_issue_id(project_id);
        let issue = store.get_issue(id).unwrap().unwrap();
        assert!(issue.body.contains("source path is a symlink"));
        assert!(issue.title.contains("codegraph"));
        assert_eq!(issue.row_version, 1);
        // A new error refreshes the same Issue.
        let changed = [notice(
            project_id,
            "codegraph source changed during indexing",
        )];
        assert_eq!(
            file_notices(&store, &changed, now + ChronoDuration::minutes(60)),
            1
        );
        let issue = store.get_issue(id).unwrap().unwrap();
        assert!(issue.body.contains("source changed during indexing"));
    }

    /// A primary and a sandbox workspace of `project`, in one manager.
    fn two_workspaces(
        project: Uuid,
    ) -> (
        crate::codegraph::IndexManager,
        IndexHandle,
        Uuid,
        Uuid,
        Vec<tempfile::TempDir>,
    ) {
        use crate::codegraph::{IndexManager, RegisteredWorkspace};
        let primary_root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::create_dir(primary_root.path().join(".git")).unwrap();
        std::fs::write(
            external.path().join(".git"),
            format!("gitdir: {}\n", primary_root.path().join(".git").display()),
        )
        .unwrap();
        let primary = RegisteredWorkspace::primary(project, primary_root.path()).unwrap();
        let sandbox = RegisteredWorkspace::registered_checkout(
            project,
            primary_root.path(),
            external.path(),
            rsi_codegraph::WorkspaceInstanceKey::RsiSandbox(Uuid::new_v4()),
        )
        .unwrap();
        let (primary_id, sandbox_id) = (primary.workspace_id, sandbox.workspace_id);
        let index = tempfile::tempdir().unwrap();
        let (manager, handle) =
            IndexManager::new(index.path().join("index"), vec![primary, sandbox]).unwrap();
        (
            manager,
            handle,
            primary_id,
            sandbox_id,
            vec![primary_root, external, index],
        )
    }

    fn fail(handle: &IndexHandle, id: Uuid, error: &str, at: chrono::DateTime<Utc>) {
        handle.with_status_for_test(id, |status| {
            status.note_run(true, true, at);
            status.phase = crate::codegraph::IndexPhase::Failed;
            status.last_error = Some(error.into());
        });
    }

    fn recover(handle: &IndexHandle, id: Uuid, at: chrono::DateTime<Utc>) {
        handle.with_status_for_test(id, |status| {
            status.note_run(false, false, at);
            status.phase = crate::codegraph::IndexPhase::Ready;
            status.last_error = None;
        });
    }

    /// #2454: an uncovered sandbox failure and a new primary episode after the
    /// health Issue was closed each reach the owner once, before the day-old
    /// escalation, and never twice over retries.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn each_failure_episode_notifies_its_owner_once_before_the_day_old_issue_2454() {
        use crate::store::manager_coordinator::tests::fixture;
        use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerPolicyV2};
        use rsi_common::types::IssueStatus;

        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(
            &store,
            ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                ..Default::default()
            },
        );
        let (job, _, _) = store.manager_action_watch_identity(&config);
        let queued = |store: &Store| store.manager_notice_undelivered_count(job).unwrap();
        let (_manager, handle, primary, sandbox, _dirs) = two_workspaces(config.project_id);
        let mut notified = HashSet::new();
        let start = Utc::now();

        // Nothing failed: nothing to tell.
        assert_eq!(
            notify_failure_episodes(&store, &handle.failure_episodes(), &mut notified),
            0
        );
        let baseline = queued(&store);

        // The sandbox checkout fails; the primary-only day-old policy ignores it.
        fail(&handle, sandbox, "codegraph source changed twice", start);
        assert!(handle.unhealthy_notices(start).is_empty());
        let episodes = handle.failure_episodes();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].checkout, "sandbox");
        assert_eq!(notify_failure_episodes(&store, &episodes, &mut notified), 1);
        assert_eq!(queued(&store), baseline + 1);
        // Retries (same process, then a restarted sweep) add nothing.
        for _ in 0..3 {
            assert_eq!(notify_failure_episodes(&store, &episodes, &mut notified), 0);
        }
        assert_eq!(
            notify_failure_episodes(&store, &episodes, &mut HashSet::new()),
            0,
            "the store dedupes an episode a restarted sweep sees again"
        );
        assert_eq!(queued(&store), baseline + 1);

        // The primary fails: owner notice now, Issue only after a day.
        fail(&handle, primary, "source path is a symlink", start);
        assert_eq!(
            notify_failure_episodes(&store, &handle.failure_episodes(), &mut notified),
            1
        );
        assert!(handle.unhealthy_notices(start).is_empty(), "under a day");
        assert_eq!(queued(&store), baseline + 2);
        let later = start + ChronoDuration::hours(25);
        let issue_id = crate::store::codegraph_health::codegraph_health_issue_id(config.project_id);
        assert_eq!(
            file_notices(&store, &handle.unhealthy_notices(later), later),
            1
        );
        let issue = store.get_issue(issue_id).unwrap().unwrap();
        assert!(issue.body.contains("source path is a symlink"));

        // The operator closes the Issue; the graph recovers and later fails anew.
        store
            .update_issue_status(issue_id, IssueStatus::Closed)
            .unwrap();
        recover(&handle, primary, later);
        assert!(
            handle
                .failure_episodes()
                .iter()
                .all(|episode| episode.workspace_id != primary)
        );
        let again = later + ChronoDuration::hours(1);
        fail(&handle, primary, "parser budget exhausted", again);
        assert_eq!(
            notify_failure_episodes(&store, &handle.failure_episodes(), &mut notified),
            1,
            "a new episode after the Issue was closed is a new signal"
        );
        assert_eq!(queued(&store), baseline + 3);
        // The closed Issue is never reopened by the new episode's escalation.
        let much_later = again + ChronoDuration::hours(25);
        assert_eq!(
            file_notices(&store, &handle.unhealthy_notices(much_later), much_later),
            0
        );
        assert_eq!(
            store.get_issue(issue_id).unwrap().unwrap().status,
            IssueStatus::Closed
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn transient_outcomes_do_not_open_an_episode_2454() {
        let mut status = crate::codegraph::IndexStatus::new(Uuid::new_v4(), Uuid::new_v4());
        status.phase = crate::codegraph::IndexPhase::Failed;
        let now = Utc::now();
        status.note_run(true, false, now);
        assert!(status.failure_episode().is_none(), "a superseded run");
        status.note_run(true, true, now);
        let first = status.failure_episode().unwrap();
        status.note_run(true, true, now + ChronoDuration::minutes(9));
        assert_eq!(
            status.failure_episode().unwrap().started_at,
            first.started_at,
            "the episode starts at its first real failure"
        );
        status.note_run(false, false, now + ChronoDuration::minutes(10));
        assert!(status.failure_episode().is_none());
    }
}
