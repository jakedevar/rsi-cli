//! Bounded, logical archive sweep. The existing delegated archive predicate is
//! the common safety gate; this job adds delivery and operator-owned holds.

use super::Store;
use crate::error::Result;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::types::{SandboxCleanupState, Session};
use rusqlite::{Transaction, TransactionBehavior, params};
use std::path::Path;
use std::process::Command;
use uuid::Uuid;

pub(crate) const RETENTION_PAGE_SIZE: usize = 64;

impl Store {
    pub(crate) fn retention_candidates(
        &self,
        after: Option<&str>,
    ) -> Result<(Vec<Uuid>, Option<String>)> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM sessions WHERE status IN ('Completed','Failed','Interrupted')
             AND (?1 IS NULL OR id>?1) ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![after, RETENTION_PAGE_SIZE as i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next = (rows.len() == RETENTION_PAGE_SIZE)
            .then(|| rows.last().cloned())
            .flatten();
        Ok((
            rows.into_iter()
                .filter_map(|id| Uuid::parse_str(&id).ok())
                .collect(),
            next,
        ))
    }

    /// Recheck under one IMMEDIATE transaction before changing a status. A
    /// false result is an exclusion; an error leaves the session untouched.
    pub(crate) fn retention_archive_one(
        &self,
        id: Uuid,
        window_hours: i64,
    ) -> Result<Option<&'static str>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(session) = self.get_session(id)? else {
            return Ok(Some("missing"));
        };
        let now = Utc::now();
        let blocker = if rsi_common::is_leaf_kind(session.session_kind) {
            self.retention_leaf_blocker(&session, now, window_hours)?
        } else {
            self.retention_container_blocker(&session, now, window_hours)?
        };
        if let Some(blocker) = blocker {
            return Ok(Some(blocker));
        }
        let changed = self.conn.execute(
            "UPDATE sessions SET status='Archived',pending_archive=0,lead_session_id=NULL,
               retry_attempt=COALESCE(max_retries,retry_attempt),updated_at=?2
             WHERE id=?1 AND status IN ('Completed','Failed','Interrupted')",
            params![
                id.to_string(),
                now.to_rfc3339_opts(SecondsFormat::Nanos, true)
            ],
        )?;
        if changed != 1 {
            return Ok(Some("state_changed"));
        }
        Store::resolve_c5_autofile_pending_tx(&tx, id)?;
        Store::cancel_queued_recovery_for_archive_on(&tx, id)?;
        tx.commit()?;
        Ok(None)
    }

    fn retention_common_blocker(
        &self,
        session: &Session,
        now: DateTime<Utc>,
        window_hours: i64,
    ) -> Result<Option<&'static str>> {
        if let Some(project) = session.project_id {
            let epic = self.manager_action_session_epic(session.id)?;
            if let Some(policy) = self.get_harness_manager_policy(project)?
                && !policy.revoked
                && (policy.policy.paused
                    || epic.is_some_and(|id| policy.policy.paused_epic_ids.contains(&id)))
            {
                return Ok(Some("manager_policy_paused"));
            }
            if let (Some(config), Some(epic)) = (self.get_harness_manager(project)?, epic)
                && self.manager_v2_action_pause(&config, epic)?.1
            {
                return Ok(Some("lead_paused"));
            }
        }
        match self.recovery_owner_gate(
            session.id,
            super::manager_actions::RecoveryOwnerMode::ManagerAction {
                allow_interrupted_resume: false,
                allow_soft_operator_pause: false,
            },
        ) {
            Ok(()) => {}
            Err(crate::error::DaemonError::InvalidParam(_)) => return Ok(Some("recovery_owner")),
            Err(error) => return Err(error),
        }
        if session.pinned_at.is_some() {
            return Ok(Some("pinned"));
        }
        if self.manager_action_operator_paused(session.id)? {
            return Ok(Some("operator_paused"));
        }
        let id = session.id.to_string();
        let wake: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs WHERE enabled=1 AND
              (wake_session_id=?1 OR wake_mode=?2))",
            params![id, format!("on_terminal:{id}")],
            |r| r.get(0),
        )?;
        if wake {
            return Ok(Some("enabled_wake"));
        }
        let review: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_review_assignments WHERE
             state IN ('reserved','allocating','active') AND
             (author_session_id=?1 OR reviewer_session_id=?1))",
            [id],
            |r| r.get(0),
        )?;
        if review {
            return Ok(Some("live_review"));
        }
        let recent = self
            .delegated_last_activity(session)?
            .is_none_or(|at| at > now - Duration::hours(window_hours));
        if recent {
            return Ok(Some("recent_activity"));
        }
        Ok(None)
    }

    fn retention_leaf_blocker(
        &self,
        session: &Session,
        now: DateTime<Utc>,
        window_hours: i64,
    ) -> Result<Option<&'static str>> {
        if self
            .delegated_archive_blocker_with_window(session, now, window_hours)?
            .is_some()
        {
            return Ok(Some("archive_gate"));
        }
        if let Some(blocker) = self.retention_common_blocker(session, now, window_hours)? {
            return Ok(Some(blocker));
        }
        if let Some(project) = session.project_id
            && self.manager_v2_session_is_sealed_source(project, session.id)?
        {
            return Ok(Some("sealed_source"));
        }
        if session.sandbox_cleanup_state == Some(SandboxCleanupState::Live)
            && !worktree_clean(session.sandbox_root.as_deref())
        {
            return Ok(Some("live_worktree"));
        }
        let filed: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM issues WHERE created_by_session_id=?1)",
            [session.id.to_string()],
            |r| r.get(0),
        )?;
        if !filed && !rolling_contains_worktree_head(session.sandbox_root.as_deref()) {
            return Ok(Some("undelivered"));
        }
        Ok(None)
    }

    fn retention_container_blocker(
        &self,
        session: &Session,
        now: DateTime<Utc>,
        window_hours: i64,
    ) -> Result<Option<&'static str>> {
        if self
            .retention_common_blocker(session, now, window_hours)?
            .is_some()
        {
            return Ok(Some("container_hold"));
        }
        for id in self
            .manager_action_descendant_ids(session.id)?
            .into_iter()
            .skip(1)
        {
            let descendant = self.get_session(id)?;
            if descendant.is_some_and(|row| {
                !matches!(
                    row.status,
                    rsi_common::types::SessionStatus::Archived
                        | rsi_common::types::SessionStatus::Deleted
                )
            }) {
                return Ok(Some("children_live"));
            }
        }
        Ok(None)
    }
}

/// Local ref is refreshed by normal rolling publication. Failure is a hold.
fn rolling_contains_worktree_head(root: Option<&Path>) -> bool {
    let Some(root) = root else {
        return false;
    };
    if !worktree_clean(Some(root)) {
        return false;
    }
    matches!(Command::new("git").args(["merge-base", "--is-ancestor", "HEAD", "refs/remotes/origin/rolling"])
        .current_dir(root).status(), Ok(status) if status.success())
}

fn worktree_clean(root: Option<&Path>) -> bool {
    let Some(root) = root else {
        return false;
    };
    if !matches!(
        crate::sandbox::git_worktree::observe_tracked_clean_idle(root),
        Ok(true)
    ) {
        return false;
    }
    let Ok(status) = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(root)
        .output()
    else {
        return false;
    };
    if !status.status.success() || !status.stdout.is_empty() {
        return false;
    }
    true
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-01")
))]
mod tests {
    use super::*;
    use rsi_common::types::{NewIssue, Project, SessionKind, SessionStatus};

    fn fixture() -> (Store, Uuid) {
        let store = Store::open_in_memory().unwrap();
        let project = Project {
            id: Uuid::new_v4(),
            name: "retention".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.insert_project(&project).unwrap();
        (store, project.id)
    }

    fn old_leaf(store: &Store, project: Uuid) -> Uuid {
        let mut session = crate::store::tests::make_test_session();
        session.project_id = Some(project);
        session.status = SessionStatus::Completed;
        session.updated_at = Utc::now() - Duration::hours(50);
        session.created_at = session.updated_at;
        store.insert_session(&session).unwrap();
        session.id
    }

    fn file_issue(store: &Store, project: Uuid, session: Uuid) {
        store
            .create_issue(&NewIssue {
                project_id: project,
                title: "Follow-up".into(),
                body: String::new(),
                priority: None,
                labels: vec![],
                created_by_session_id: Some(session),
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn filed_idle_leaf_archives_logically_and_holds_are_preserved() {
        let (store, project) = fixture();
        let undelivered = old_leaf(&store, project);
        assert_eq!(
            store.retention_archive_one(undelivered, 24).unwrap(),
            Some("undelivered")
        );
        assert_eq!(
            store.get_session(undelivered).unwrap().unwrap().status,
            SessionStatus::Completed
        );

        let filed = old_leaf(&store, project);
        file_issue(&store, project, filed);
        assert_eq!(store.retention_archive_one(filed, 24).unwrap(), None);
        assert_eq!(
            store.get_session(filed).unwrap().unwrap().status,
            SessionStatus::Archived
        );

        let pinned = old_leaf(&store, project);
        file_issue(&store, project, pinned);
        store.toggle_session_pin(pinned).unwrap();
        assert!(store.retention_archive_one(pinned, 24).unwrap().is_some());
        assert_eq!(
            store.get_session(pinned).unwrap().unwrap().status,
            SessionStatus::Completed
        );

        let paused = old_leaf(&store, project);
        file_issue(&store, project, paused);
        store.record_manager_operator_pause(paused, true).unwrap();
        assert!(store.retention_archive_one(paused, 24).unwrap().is_some());
        assert_eq!(
            store.get_session(paused).unwrap().unwrap().status,
            SessionStatus::Completed
        );

        let recent = old_leaf(&store, project);
        file_issue(&store, project, recent);
        assert!(store.retention_archive_one(recent, 72).unwrap().is_some());
        assert_eq!(
            store.get_session(recent).unwrap().unwrap().status,
            SessionStatus::Completed
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn terminal_container_waits_for_children_then_cascades() {
        let (store, project) = fixture();
        let mut container = crate::store::tests::make_test_session();
        container.project_id = Some(project);
        container.session_kind = SessionKind::Group;
        container.status = SessionStatus::Completed;
        container.updated_at = Utc::now() - Duration::hours(50);
        container.created_at = container.updated_at;
        store.insert_session(&container).unwrap();
        let mut child = crate::store::tests::make_test_session();
        child.project_id = Some(project);
        child.parent_id = Some(container.id);
        child.status = SessionStatus::Completed;
        child.updated_at = container.updated_at;
        child.created_at = container.created_at;
        store.insert_session(&child).unwrap();
        file_issue(&store, project, child.id);
        assert!(
            store
                .retention_archive_one(container.id, 24)
                .unwrap()
                .is_some()
        );
        assert_eq!(store.retention_archive_one(child.id, 24).unwrap(), None);
        assert_eq!(store.retention_archive_one(container.id, 24).unwrap(), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn candidate_page_is_bounded_and_advances() {
        let (store, project) = fixture();
        for _ in 0..(RETENTION_PAGE_SIZE + 1) {
            old_leaf(&store, project);
        }
        let (first, cursor) = store.retention_candidates(None).unwrap();
        assert_eq!(first.len(), RETENTION_PAGE_SIZE);
        let (second, end) = store.retention_candidates(cursor.as_deref()).unwrap();
        assert_eq!(second.len(), 1);
        assert!(end.is_none());
        assert!(!first.contains(&second[0]));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn resume_wake_human_gate_and_untracked_worktree_hold_filed_leaf() {
        let (store, project) = fixture();
        let stamp = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let waking = old_leaf(&store, project);
        file_issue(&store, project, waking);
        store.conn.execute(
            "INSERT INTO scheduled_jobs(id,name,message,schedule_json,next_fire_at,enabled,created_at,updated_at,wake_mode,wake_session_id)
             VALUES(?1,'resume','continue','{}',?2,1,?2,?2,'resume',?3)",
            params![Uuid::new_v4().to_string(), stamp, waking.to_string()],
        ).unwrap();
        assert!(store.retention_archive_one(waking, 24).unwrap().is_some());

        let gated = old_leaf(&store, project);
        file_issue(&store, project, gated);
        store
            .conn
            .execute(
                "UPDATE sessions SET pending_archive=1 WHERE id=?1",
                [gated.to_string()],
            )
            .unwrap();
        assert!(store.retention_archive_one(gated, 24).unwrap().is_some());

        let dirty = old_leaf(&store, project);
        file_issue(&store, project, dirty);
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=Retention Test",
                    "-c",
                    "user.email=retention@example.invalid",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "base"
                ])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(dir.path().join("untracked.txt"), "pending").unwrap();
        store.conn.execute(
            "UPDATE sessions SET sandbox_kind='GitWorktree',sandbox_root=?2,sandbox_cleanup_state='Live' WHERE id=?1",
            params![dirty.to_string(), dir.path().to_str().unwrap()],
        ).unwrap();
        assert_eq!(
            store.retention_archive_one(dirty, 24).unwrap(),
            Some("live_worktree")
        );
        for id in [waking, gated, dirty] {
            assert_eq!(
                store.get_session(id).unwrap().unwrap().status,
                SessionStatus::Completed
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn clean_worktree_head_archives_only_after_rolling_contains_it() {
        let (store, project) = fixture();
        let source = old_leaf(&store, project);
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["init", "--quiet"]));
        assert!(git(&[
            "-c",
            "user.name=Retention Test",
            "-c",
            "user.email=retention@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "base"
        ]));
        assert!(git(&["update-ref", "refs/remotes/origin/rolling", "HEAD"]));
        assert!(git(&[
            "-c",
            "user.name=Retention Test",
            "-c",
            "user.email=retention@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "source"
        ]));
        store.conn.execute(
            "UPDATE sessions SET sandbox_kind='GitWorktree',sandbox_root=?2,sandbox_cleanup_state='Live' WHERE id=?1",
            params![source.to_string(), dir.path().to_str().unwrap()],
        ).unwrap();
        assert_eq!(
            store.retention_archive_one(source, 24).unwrap(),
            Some("undelivered")
        );
        assert!(git(&["update-ref", "refs/remotes/origin/rolling", "HEAD"]));
        assert_eq!(store.retention_archive_one(source, 24).unwrap(), None);
        assert_eq!(
            store.get_session(source).unwrap().unwrap().status,
            SessionStatus::Archived
        );
    }
}
