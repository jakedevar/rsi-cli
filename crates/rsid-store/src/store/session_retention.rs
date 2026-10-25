//! Bounded, logical archive sweep. The existing delegated archive predicate is
//! the common safety gate; this job adds delivery and operator-owned holds.

use super::Store;
use crate::error::Result;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::types::{SandboxCleanupState, Session};
use rusqlite::{Transaction, TransactionBehavior, params};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

pub(crate) const RETENTION_PAGE_SIZE: usize = 64;

impl Store {
    pub fn retention_candidates(&self, after: Option<&str>) -> Result<(Vec<Uuid>, Option<String>)> {
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
    ///
    /// Runs the git probes inline, so the caller holds the Store for the
    /// subprocesses: the periodic pass uses [`Self::retention_plan_one`], the
    /// probe outside the lock and [`Self::retention_finish_one`] instead.
    pub fn retention_archive_one(
        &self,
        id: Uuid,
        window_hours: i64,
    ) -> Result<Option<&'static str>> {
        let needs = match self.retention_plan_one(id, window_hours)? {
            RetentionPlan::Held(reason) => return Ok(Some(reason)),
            RetentionPlan::Probe(needs) => needs,
        };
        self.retention_finish_one(id, window_hours, &needs.probe())
    }

    /// Phase 1, read-only: evaluate every gate that needs no subprocess. A
    /// candidate a database gate holds is `Held`; one that passes them all
    /// returns the git facts the final gates will need (possibly none), which
    /// the caller gathers with [`RetentionGitNeeds::probe`] without the Store
    /// lock and hands to [`Self::retention_finish_one`].
    pub fn retention_plan_one(&self, id: Uuid, window_hours: i64) -> Result<RetentionPlan> {
        let needs = RefCell::new(RetentionGitNeeds::default());
        let verdict = self.retention_verdict(id, window_hours, &GitSource::Collect(&needs))?;
        Ok(match verdict {
            Some(reason) => RetentionPlan::Held(reason),
            None => RetentionPlan::Probe(needs.into_inner()),
        })
    }

    /// Phase 3: the full gate chain again under one IMMEDIATE transaction with
    /// the probed git facts, then the status change. A fact that no longer
    /// matches the row (the root moved, a fact is missing) holds the session as
    /// `probe_stale` for the next pass; an archive gate is never skipped.
    pub fn retention_finish_one(
        &self,
        id: Uuid,
        window_hours: i64,
        git: &RetentionGitFacts,
    ) -> Result<Option<&'static str>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(reason) = self.retention_verdict(id, window_hours, &GitSource::Probed(git))? {
            return Ok(Some(reason));
        }
        let now = Utc::now();
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

    /// The gate chain shared by both phases: `None` means every gate passed.
    fn retention_verdict(
        &self,
        id: Uuid,
        window_hours: i64,
        git: &GitSource<'_>,
    ) -> Result<Option<&'static str>> {
        let Some(session) = self.get_session(id)? else {
            return Ok(Some("missing"));
        };
        let now = Utc::now();
        if rsi_common::is_leaf_kind(session.session_kind) {
            return self.retention_leaf_blocker(&session, now, window_hours, git);
        }
        self.retention_container_blocker(&session, now, window_hours)
    }

    /// A digest of the effective retention window, session row stamps, wake /
    /// review / Issue / pause facts, the latest eligible Issue binding and its
    /// Issue state, and descendant identities/statuses for containers. A terminal
    /// session gets a new event only by resuming, which moves its row.
    /// Elapsed time and external git state are not fingerprinted; the capped
    /// hold backoff still re-evaluates them, and a skip never archives anything.
    pub fn retention_input_fingerprint(&self, id: Uuid, window_hours: i64) -> Result<String> {
        let sid = id.to_string();
        let (fingerprint, kind) = self.conn.query_row(
            "SELECT ?4||'|'||COALESCE(s.status,'')||'|'||COALESCE(s.updated_at,'')||'|'
               ||COALESCE(s.pinned_at,'')||'|'||COALESCE(s.pending_archive,'')||'|'
               ||COALESCE(s.sandbox_root,'')||'|'||COALESCE(s.sandbox_cleanup_state,'')||'|'
               ||COALESCE(s.parent_id,'')||'|'||COALESCE(s.lead_session_id,'')||'|'
               ||(s.pending_question_json IS NOT NULL)||'|'
               ||(SELECT COUNT(*)||':'||COALESCE(MAX(updated_at),'') FROM scheduled_jobs
                   WHERE wake_session_id=?1 OR wake_mode=?2)||'|'
               ||(SELECT COUNT(*)||':'||COALESCE(MAX(updated_at),'')
                   FROM manager_review_assignments
                   WHERE author_session_id=?1 OR reviewer_session_id=?1)||'|'
               ||(SELECT COUNT(*) FROM issues WHERE created_by_session_id=?1)||'|'
               ||COALESCE((SELECT json_array(i.id,i.status,i.archived_at,i.updated_at,
                     (SELECT MAX(n.rowid) FROM harness_manager_v2_operations n
                      WHERE n.kind='lifecycle_action'
                        AND json_extract(n.payload_json,'$.issue_binding.issue_id')=i.id))
                   FROM harness_manager_v2_operations o
                   JOIN issues i ON i.project_id=o.project_id
                    AND i.id=json_extract(o.payload_json,'$.issue_binding.issue_id')
                   WHERE o.id=(SELECT b.id FROM harness_manager_v2_operations b
                     WHERE b.kind='lifecycle_action' AND b.target_session_id=?1
                       AND b.state IN ('running','succeeded')
                       AND json_extract(b.payload_json,'$.issue_binding') IS NOT NULL
                     ORDER BY b.rowid DESC LIMIT 1)), '')||'|'
               ||(SELECT COUNT(*)||':'||COALESCE(MAX(updated_at),'')
                   FROM harness_manager_v2_operations WHERE target_session_id=?1)||'|'
               ||COALESCE((SELECT value FROM daemon_settings WHERE key=?3),'')||'|'
               ||COALESCE((SELECT row_version||':'||updated_at
                   FROM harness_manager_v2_policies WHERE project_id=s.project_id),'')
               ,s.session_kind
             FROM sessions s WHERE s.id=?1",
            params![
                sid,
                format!("on_terminal:{sid}"),
                format!("manager_operator_pause:{sid}"),
                window_hours
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        let kind = serde_json::from_value(serde_json::Value::String(kind))?;
        let mut digest = blake3::Hasher::new();
        digest.update(fingerprint.as_bytes());
        if !rsi_common::is_leaf_kind(kind) {
            // Use the gate's bounded traversal, including its cycle/size checks.
            // Sorting makes the digest independent of traversal order.
            let mut descendants = self.manager_action_descendant_ids(id)?;
            descendants.sort_unstable();
            let mut status = self
                .conn
                .prepare("SELECT status FROM sessions WHERE id=?1")?;
            for child in descendants.into_iter().filter(|child| *child != id) {
                let state: String = status.query_row([child.to_string()], |row| row.get(0))?;
                digest.update(child.as_bytes());
                digest.update(state.as_bytes());
                digest.update(&[0]);
            }
        }
        Ok(digest.finalize().to_hex().to_string())
    }

    /// #247: why an AUTOMATIC archive or sandbox reclaim must leave a terminal
    /// but resumable session alone: the operator paused it, or it is a worker
    /// bound to an Issue that is still `InProgress` with no final handoff (a
    /// manager halt for higher-priority work looks exactly like a finished
    /// Interrupted session once its tree is clean and in Git). The retention
    /// pass and the source-worktree settlement share this one predicate; an
    /// explicit archive does not consult it, and a finished worker, a closed or
    /// relaunched Issue and an archived session release it, so nothing is
    /// retained indefinitely.
    pub fn automatic_reclaim_resume_hold(&self, session_id: Uuid) -> Result<Option<&'static str>> {
        // #3908: a completed provider turn does not retire an Epic lead. It
        // may be waiting for manager mail without a scheduled wake. Keep the
        // checkout until the lead is explicitly replaced, unassigned or archived.
        let live_lead: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions epic JOIN sessions lead
               ON lead.id=epic.lead_session_id
             WHERE lead.id=?1 AND epic.session_kind='Epic'
               AND epic.status NOT IN ('Archived','Deleted')
               AND lead.status NOT IN ('Archived','Deleted'))",
            [session_id.to_string()],
            |row| row.get(0),
        )?;
        if live_lead {
            return Ok(Some("live_epic_lead"));
        }
        if self.manager_action_operator_paused(session_id)? {
            return Ok(Some("operator_paused"));
        }
        if let Some(pause) = self.automatic_reclaim_pause_hold(session_id)? {
            return Ok(Some(pause));
        }
        if self.unfinished_issue_worker_hold(session_id)? {
            return Ok(Some("unfinished_issue_worker"));
        }
        Ok(None)
    }

    /// The manager-side pauses that cover a session without any per-worker
    /// operator marker: the project policy's `paused` / `paused_epic_ids`
    /// (while the grant is not revoked) and the Epic's lead pause. A paused
    /// Epic's workers are resumable work, so neither the logical archive nor
    /// the source-worktree settlement may reclaim them (#2683). Read-only; it
    /// is part of [`Self::automatic_reclaim_resume_hold`], so the settlement
    /// audit and its pre-effect fence see a pause that arrives after intent.
    fn automatic_reclaim_pause_hold(&self, session_id: Uuid) -> Result<Option<&'static str>> {
        let Some(project) = self.get_session(session_id)?.and_then(|s| s.project_id) else {
            return Ok(None);
        };
        let epic = self.manager_action_session_epic(session_id)?;
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
        Ok(None)
    }

    fn retention_common_blocker(
        &self,
        session: &Session,
        now: DateTime<Utc>,
        window_hours: i64,
    ) -> Result<Option<&'static str>> {
        if let Some(pause) = self.automatic_reclaim_pause_hold(session.id)? {
            return Ok(Some(pause));
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
        if let Some(hold) = self.automatic_reclaim_resume_hold(session.id)? {
            return Ok(Some(hold));
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
        git: &GitSource<'_>,
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
        let root = session.sandbox_root.as_deref();
        if session.sandbox_cleanup_state == Some(SandboxCleanupState::Live) {
            match git.worktree_clean(root) {
                None => return Ok(Some("probe_stale")),
                Some(false) => return Ok(Some("live_worktree")),
                Some(true) => {}
            }
        }
        let filed: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM issues WHERE created_by_session_id=?1)",
            [session.id.to_string()],
            |r| r.get(0),
        )?;
        if filed {
            return Ok(None);
        }
        match git.delivered(root) {
            None => Ok(Some("probe_stale")),
            Some(false) => Ok(Some("undelivered")),
            Some(true) => Ok(None),
        }
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

/// What phase 1 found the final gates will ask git: the sandbox root and
/// which probes. Owned data, so the probe can run on a blocking thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionGitNeeds {
    root: Option<PathBuf>,
    live_worktree: bool,
    delivery: bool,
}

impl RetentionGitNeeds {
    /// No subprocess is needed: phase 3 can follow phase 1 at once.
    pub fn is_empty(&self) -> bool {
        !self.live_worktree && !self.delivery
    }

    /// Run the probes. Blocking (`git status`, `git merge-base`): call it with
    /// no Store lock held. The delivery probe reuses the cleanliness verdict
    /// when this call already computed it, so the tree is read once.
    pub fn probe(&self) -> RetentionGitFacts {
        let root = self.root.as_deref();
        let clean = self.live_worktree.then(|| worktree_clean(root));
        let delivered = self.delivery.then(|| {
            let clean = clean.unwrap_or_else(|| worktree_clean(root));
            clean && root.is_some_and(rolling_contains_head)
        });
        RetentionGitFacts {
            root: self.root.clone(),
            worktree_clean: clean,
            delivered,
        }
    }
}

/// The probed git facts for one candidate, bound to the root they describe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionGitFacts {
    root: Option<PathBuf>,
    worktree_clean: Option<bool>,
    delivered: Option<bool>,
}

/// Phase 1 of a candidate's evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionPlan {
    /// A database gate holds the session.
    Held(&'static str),
    /// Every database gate passed; these git facts decide the rest.
    Probe(RetentionGitNeeds),
}

/// Where the leaf gates get their git facts: phase 1 records what it needs and
/// assumes it passes; phase 3 reads the probed answer.
enum GitSource<'a> {
    Collect(&'a RefCell<RetentionGitNeeds>),
    Probed(&'a RetentionGitFacts),
}

impl GitSource<'_> {
    /// `None` is a probed fact that is missing or describes another root.
    fn worktree_clean(&self, root: Option<&Path>) -> Option<bool> {
        let Some(root) = root else {
            return Some(false);
        };
        match self {
            Self::Collect(needs) => {
                let mut needs = needs.borrow_mut();
                needs.root = Some(root.to_path_buf());
                needs.live_worktree = true;
                Some(true)
            }
            Self::Probed(facts) if facts.root.as_deref() == Some(root) => facts.worktree_clean,
            Self::Probed(_) => None,
        }
    }

    fn delivered(&self, root: Option<&Path>) -> Option<bool> {
        let Some(root) = root else {
            return Some(false);
        };
        match self {
            Self::Collect(needs) => {
                let mut needs = needs.borrow_mut();
                needs.root = Some(root.to_path_buf());
                needs.delivery = true;
                Some(true)
            }
            Self::Probed(facts) if facts.root.as_deref() == Some(root) => facts.delivered,
            Self::Probed(_) => None,
        }
    }
}

/// Local ref is refreshed by normal rolling publication. Failure is a hold.
/// The caller has already established a clean tree.
fn rolling_contains_head(root: &Path) -> bool {
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
    use rsi_common::types::{
        ConversationEvent, EventType, NewIssue, Project, Role, SessionKind, SessionStatus,
    };

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

    /// An Epic with a manager appointed over it, and a finished, filed leaf
    /// under the Epic: only the project policy can hold the leaf.
    fn leaf_under_managed_epic(store: &Store, project: Uuid) -> (Uuid, Uuid, Uuid) {
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        let mut manager = crate::store::tests::make_test_session();
        manager.project_id = Some(project);
        manager.session_kind = SessionKind::Standard;
        manager.status = SessionStatus::Completed;
        store.insert_session(&manager).unwrap();
        let mut group = manager.clone();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        store.insert_session(&group).unwrap();
        let mut epic = manager.clone();
        epic.id = Uuid::new_v4();
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Running;
        epic.parent_id = Some(group.id);
        store.insert_session(&epic).unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: manager.id,
                epic_ids: Some(vec![epic.id]),
                expected_row_version: 0,
            })
            .unwrap();
        let leaf = old_leaf(store, project);
        store
            .conn
            .execute(
                "UPDATE sessions SET parent_id=?2 WHERE id=?1",
                params![leaf.to_string(), epic.id.to_string()],
            )
            .unwrap();
        file_issue(store, project, leaf);
        (manager.id, epic.id, leaf)
    }

    fn set_policy(
        store: &Store,
        project: Uuid,
        version: i64,
        paused: bool,
        paused_epic_ids: Vec<Uuid>,
    ) {
        use rsi_common::harness_manager_v2::{
            ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerPolicyV2,
        };
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: 1,
                expected_policy_version: version,
                idempotency_key: format!("policy-{version}"),
                policy: ManagerPolicyV2 {
                    capabilities: vec![ManagerCapabilityV2::WorkPlan],
                    paused,
                    paused_epic_ids,
                    ..Default::default()
                },
            })
            .unwrap();
    }

    /// #2683: a leaf covered only by its Epic's (or the project's) policy pause
    /// has no per-worker operator marker and no unfinished Issue, yet the shared
    /// automatic-reclaim predicate (which the settlement audit and its
    /// pre-effect fence consult) must hold it, exactly as the archive does; an
    /// explicit archive does not consult it, and lifting the pause releases it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn an_epic_or_project_policy_pause_holds_the_shared_automatic_reclaim_predicate() {
        let (store, project) = fixture();
        let (_manager, epic, leaf) = leaf_under_managed_epic(&store, project);
        set_policy(&store, project, 0, false, Vec::new());
        assert_eq!(store.automatic_reclaim_resume_hold(leaf).unwrap(), None);

        set_policy(&store, project, 1, false, vec![epic]);
        assert_eq!(
            store.automatic_reclaim_resume_hold(leaf).unwrap(),
            Some("manager_policy_paused"),
            "the Epic pause alone holds the worker"
        );
        assert_eq!(
            store.retention_archive_one(leaf, 24).unwrap(),
            Some("manager_policy_paused")
        );
        // A session outside the paused Epic is not held by it.
        let outside = old_leaf(&store, project);
        assert_eq!(store.automatic_reclaim_resume_hold(outside).unwrap(), None);

        set_policy(&store, project, 2, true, Vec::new());
        assert_eq!(
            store.automatic_reclaim_resume_hold(outside).unwrap(),
            Some("manager_policy_paused"),
            "the project-wide pause holds every worker of the project"
        );

        set_policy(&store, project, 3, false, Vec::new());
        assert_eq!(store.automatic_reclaim_resume_hold(leaf).unwrap(), None);
        assert_eq!(store.retention_archive_one(leaf, 24).unwrap(), None);
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

    /// #247: a manager-halted worker whose Issue is still `InProgress` with no
    /// final report in its latest turn is paused work, whatever Issue handoffs
    /// it published. Its clean, delivered tree does not let the automatic
    /// archive retire it (that would purge the root the manager plans to
    /// resume); a current final report or a closed Issue releases it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn an_unfinished_issue_worker_is_not_archived_until_it_reports_or_its_issue_ends() {
        let (store, project) = fixture();
        let manager = old_leaf(&store, project);
        let issue_for = |status: &str| {
            let issue = store
                .create_issue(&NewIssue {
                    project_id: project,
                    title: "bound Issue".into(),
                    body: String::new(),
                    priority: None,
                    labels: vec![],
                    created_by_session_id: None,
                    assignee: None,
                    idea_id: None,
                    source_event_id: None,
                    source_finding_ref: None,
                })
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE issues SET status=?2 WHERE id=?1",
                    params![issue.id.to_string(), status],
                )
                .unwrap();
            issue.id
        };
        let halted_worker = |issue: Uuid| {
            let worker = old_leaf(&store, project);
            file_issue(&store, project, worker);
            store
                .update_session_status(worker, SessionStatus::Interrupted)
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE sessions SET updated_at=?2 WHERE id=?1",
                    params![
                        worker.to_string(),
                        (Utc::now() - Duration::hours(50))
                            .to_rfc3339_opts(SecondsFormat::Nanos, true)
                    ],
                )
                .unwrap();
            let stamp = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
            let payload = serde_json::json!({
                "request": {"operation": {"action": "create_session"}},
                "issue_binding": {"issue_id": issue.to_string()},
            });
            store
                .conn
                .execute(
                    "INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,
                        scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,
                        state,target_session_id,outcome_json,not_before,created_at,updated_at)
                     VALUES(?1,?2,?3,1,1,?1,'fixture','lifecycle_action',?4,'succeeded',?5,'{}',?6,?6,?6)",
                    params![
                        Uuid::new_v4().to_string(),
                        project.to_string(),
                        manager.to_string(),
                        payload.to_string(),
                        worker.to_string(),
                        stamp
                    ],
                )
                .unwrap();
            worker
        };

        let issue = issue_for("InProgress");
        let paused = halted_worker(issue);
        assert_eq!(
            store.retention_archive_one(paused, 24).unwrap(),
            Some("unfinished_issue_worker")
        );
        assert_eq!(
            store.get_session(paused).unwrap().unwrap().status,
            SessionStatus::Interrupted
        );
        let held = store.retention_input_fingerprint(paused, 24).unwrap();
        assert_eq!(held, store.retention_input_fingerprint(paused, 24).unwrap());
        let stamp = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "UPDATE issues SET updated_at=?2 WHERE id=?1",
                params![issue.to_string(), stamp],
            )
            .unwrap();
        let updated = store.retention_input_fingerprint(paused, 24).unwrap();
        assert_ne!(held, updated);
        store
            .conn
            .execute(
                "UPDATE issues SET archived_at=?2 WHERE id=?1",
                params![issue.to_string(), stamp],
            )
            .unwrap();
        let archived = store.retention_input_fingerprint(paused, 24).unwrap();
        assert_ne!(updated, archived);
        store
            .conn
            .execute(
                "UPDATE issues SET archived_at=NULL WHERE id=?1",
                [issue.to_string()],
            )
            .unwrap();
        assert_eq!(
            updated,
            store.retention_input_fingerprint(paused, 24).unwrap()
        );
        store
            .conn
            .execute(
                "UPDATE issues SET status='Closed' WHERE id=?1",
                [issue.to_string()],
            )
            .unwrap();
        assert_ne!(
            updated,
            store.retention_input_fingerprint(paused, 24).unwrap()
        );
        assert_eq!(store.retention_archive_one(paused, 24).unwrap(), None);
        assert_eq!(
            store.get_session(paused).unwrap().unwrap().status,
            SessionStatus::Archived
        );

        // A final assistant report in the latest turn (the existing report
        // contract) finishes the worker; the Issue stays InProgress.
        let issue = issue_for("InProgress");
        let finished = halted_worker(issue);
        let event = |worker: Uuid, sequence: i32, role: Role, content: &str, hours: i64| {
            store
                .insert_event(&ConversationEvent {
                    id: 0,
                    session_id: worker,
                    sequence,
                    event_type: EventType::Message,
                    role: Some(role),
                    content: content.into(),
                    tool_name: None,
                    tool_input: None,
                    created_at: Utc::now() - Duration::hours(hours),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .unwrap();
        };
        assert_eq!(
            store.retention_archive_one(finished, 24).unwrap(),
            Some("unfinished_issue_worker")
        );
        event(
            finished,
            1,
            Role::Assistant,
            "PIPELINE HANDOFF — IMPLEMENTATION:\nRESULT: done",
            48,
        );
        assert_eq!(store.retention_archive_one(finished, 24).unwrap(), None);
        assert_eq!(
            store.get_session(finished).unwrap().unwrap().status,
            SessionStatus::Archived
        );

        // An OLD (pre-verification) handoff, then the manager's Continue, then
        // a halt: the Issue is unfinished again, so the worker stays held.
        let issue = issue_for("InProgress");
        let continued = halted_worker(issue);
        let at = |hours: i64| {
            (Utc::now() - Duration::hours(hours)).to_rfc3339_opts(SecondsFormat::Nanos, true)
        };
        store
            .conn
            .execute(
                "INSERT INTO issue_handoffs
                     (issue_id, sequence, project_id, author_session_id, idempotency_key,
                      body, created_at)
                 VALUES (?1, 1, ?2, ?3, 'handoff-247', 'PREVERIFICATION', ?4)",
                params![
                    issue.to_string(),
                    project.to_string(),
                    continued.to_string(),
                    at(49)
                ],
            )
            .unwrap();
        event(
            continued,
            1,
            Role::User,
            "Continue: correct the review finding",
            48,
        );
        // A handoff published in the current turn without a final assistant
        // report is progress, not completion.
        store
            .conn
            .execute(
                "INSERT INTO issue_handoffs
                     (issue_id, sequence, project_id, author_session_id, idempotency_key,
                      body, created_at)
                 VALUES (?1, 2, ?2, ?3, 'handoff-247-2', 'WIP', ?4)",
                params![
                    issue.to_string(),
                    project.to_string(),
                    continued.to_string(),
                    at(47)
                ],
            )
            .unwrap();
        assert_eq!(
            store.retention_archive_one(continued, 24).unwrap(),
            Some("unfinished_issue_worker")
        );
        assert_eq!(
            store.get_session(continued).unwrap().unwrap().status,
            SessionStatus::Interrupted
        );
    }

    /// #1098: a Completed worker with commits not on rolling and an armed
    /// manager watch stays continuable across a cleanup pass, even when it
    /// filed an Issue (which otherwise exempts the delivery check).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn unlanded_worker_with_armed_manager_watch_stays_continuable() {
        let (store, project) = fixture();
        let manager = old_leaf(&store, project);
        let worker = old_leaf(&store, project);
        file_issue(&store, project, worker);
        store
            .conn
            .execute(
                "UPDATE sessions SET parent_id=?2 WHERE id=?1",
                params![worker.to_string(), manager.to_string()],
            )
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args([
                    "-c",
                    "user.name=Retention Test",
                    "-c",
                    "user.email=r@example.invalid",
                ])
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["init", "--quiet"]));
        assert!(git(&["commit", "--quiet", "--allow-empty", "-m", "base"]));
        assert!(git(&["update-ref", "refs/remotes/origin/rolling", "HEAD"]));
        assert!(git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "unlanded work"
        ]));
        store.conn.execute(
            "UPDATE sessions SET sandbox_kind='GitWorktree',sandbox_root=?2,sandbox_cleanup_state='Live' WHERE id=?1",
            params![worker.to_string(), dir.path().to_str().unwrap()],
        ).unwrap();
        let stamp = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let watch = Uuid::new_v4();
        store.conn.execute(
            "INSERT INTO scheduled_jobs(id,name,message,schedule_json,next_fire_at,enabled,created_at,updated_at,wake_mode,wake_session_id)
             VALUES(?1,'watch','report','{}',?2,1,?2,?2,?3,?4)",
            params![watch.to_string(), stamp, format!("on_terminal:{worker}"), manager.to_string()],
        ).unwrap();
        assert!(store.retention_archive_one(worker, 24).unwrap().is_some());
        assert_eq!(
            store.get_session(worker).unwrap().unwrap().status,
            SessionStatus::Completed
        );
    }

    /// A clean git repository whose HEAD is already on `refs/remotes/origin/rolling`,
    /// attached to `session` as its live worktree.
    fn delivered_worktree(store: &Store, session: Uuid) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args([
                        "-c",
                        "user.name=Retention Test",
                        "-c",
                        "user.email=r@example.invalid",
                    ])
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "base"]);
        git(&["update-ref", "refs/remotes/origin/rolling", "HEAD"]);
        store.conn.execute(
            "UPDATE sessions SET sandbox_kind='GitWorktree',sandbox_root=?2,sandbox_cleanup_state='Live' WHERE id=?1",
            params![session.to_string(), dir.path().to_str().unwrap()],
        ).unwrap();
        dir
    }

    /// Hub #3030: phase 1 runs no subprocess and names what git must answer;
    /// phase 3 archives from those facts.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn plan_names_the_git_probes_and_finish_archives_from_their_facts() {
        let (store, project) = fixture();
        let leaf = old_leaf(&store, project);
        let _dir = delivered_worktree(&store, leaf);
        let RetentionPlan::Probe(needs) = store.retention_plan_one(leaf, 24).unwrap() else {
            panic!("a leaf that passes the database gates waits for git");
        };
        assert!(needs.live_worktree && needs.delivery && !needs.is_empty());
        // Phase 1 changed nothing.
        assert_eq!(
            store.get_session(leaf).unwrap().unwrap().status,
            SessionStatus::Completed
        );
        let facts = needs.probe();
        assert_eq!(facts.worktree_clean, Some(true));
        assert_eq!(facts.delivered, Some(true));
        assert_eq!(store.retention_finish_one(leaf, 24, &facts).unwrap(), None);
        assert_eq!(
            store.get_session(leaf).unwrap().unwrap().status,
            SessionStatus::Archived
        );
    }

    /// The IMMEDIATE re-check keeps every gate: a hold that arrives after the
    /// probe, or probed facts that describe another root, stop the archive.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn finish_rechecks_every_gate_and_refuses_facts_for_another_root() {
        let (store, project) = fixture();
        let leaf = old_leaf(&store, project);
        let _dir = delivered_worktree(&store, leaf);
        let RetentionPlan::Probe(needs) = store.retention_plan_one(leaf, 24).unwrap() else {
            panic!("expected a probe plan");
        };
        let facts = needs.probe();

        // Facts probed for a different root are stale, not trusted.
        let other = tempfile::tempdir().unwrap();
        let mut elsewhere = facts.clone();
        elsewhere.root = Some(other.path().to_path_buf());
        assert_eq!(
            store.retention_finish_one(leaf, 24, &elsewhere).unwrap(),
            Some("probe_stale")
        );
        // A missing fact is stale too.
        assert_eq!(
            store
                .retention_finish_one(leaf, 24, &RetentionGitFacts::default())
                .unwrap(),
            Some("probe_stale")
        );

        // An operator pin placed between the probe and the update holds it.
        store.toggle_session_pin(leaf).unwrap();
        assert!(
            store
                .retention_finish_one(leaf, 24, &facts)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store.get_session(leaf).unwrap().unwrap().status,
            SessionStatus::Completed
        );
        store.toggle_session_pin(leaf).unwrap();
        assert_eq!(store.retention_finish_one(leaf, 24, &facts).unwrap(), None);
    }

    /// A candidate held by a database gate never reaches git, and a leaf with
    /// no worktree needs no probe at all.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn database_holds_and_worktreeless_leaves_need_no_probe() {
        let (store, project) = fixture();
        let pinned = old_leaf(&store, project);
        file_issue(&store, project, pinned);
        store.toggle_session_pin(pinned).unwrap();
        assert!(matches!(
            store.retention_plan_one(pinned, 24).unwrap(),
            RetentionPlan::Held(_)
        ));
        let filed = old_leaf(&store, project);
        file_issue(&store, project, filed);
        let RetentionPlan::Probe(needs) = store.retention_plan_one(filed, 24).unwrap() else {
            panic!("a filed leaf passes the database gates");
        };
        assert!(needs.is_empty());
        assert_eq!(
            store
                .retention_finish_one(filed, 24, &needs.probe())
                .unwrap(),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn input_fingerprint_changes_with_a_gate_input_and_not_without_one() {
        let (store, project) = fixture();
        let leaf = old_leaf(&store, project);
        let other = old_leaf(&store, project);
        let before = store.retention_input_fingerprint(leaf, 24).unwrap();
        assert_eq!(before, store.retention_input_fingerprint(leaf, 24).unwrap());
        assert_ne!(before, store.retention_input_fingerprint(leaf, 72).unwrap());
        store.toggle_session_pin(other).unwrap();
        assert_eq!(
            before,
            store.retention_input_fingerprint(leaf, 24).unwrap(),
            "another session's change is not this candidate's input"
        );
        file_issue(&store, project, leaf);
        let filed = store.retention_input_fingerprint(leaf, 24).unwrap();
        assert_ne!(before, filed);
        store.toggle_session_pin(leaf).unwrap();
        let pinned = store.retention_input_fingerprint(leaf, 24).unwrap();
        assert_ne!(filed, pinned);
        store.record_manager_operator_pause(leaf, true).unwrap();
        assert_ne!(pinned, store.retention_input_fingerprint(leaf, 24).unwrap());
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn container_fingerprint_tracks_nested_descendant_status_and_membership() {
        let (store, project) = fixture();
        let root = old_leaf(&store, project);
        let child = old_leaf(&store, project);
        let grandchild = old_leaf(&store, project);
        store
            .conn
            .execute(
                "UPDATE sessions SET session_kind='Group' WHERE id=?1",
                [root.to_string()],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET session_kind='Epic',parent_id=?2 WHERE id=?1",
                params![child.to_string(), root.to_string()],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET parent_id=?2 WHERE id=?1",
                params![grandchild.to_string(), child.to_string()],
            )
            .unwrap();
        assert_eq!(
            store.retention_plan_one(root, 24).unwrap(),
            RetentionPlan::Held("children_live")
        );
        let before = store.retention_input_fingerprint(root, 24).unwrap();
        assert_eq!(before, store.retention_input_fingerprint(root, 24).unwrap());
        store
            .update_session_status(grandchild, SessionStatus::Archived)
            .unwrap();
        let nested = store.retention_input_fingerprint(root, 24).unwrap();
        assert_ne!(before, nested);
        store
            .update_session_status(child, SessionStatus::Archived)
            .unwrap();
        let finished = store.retention_input_fingerprint(root, 24).unwrap();
        assert_ne!(nested, finished);
        assert!(matches!(
            store.retention_plan_one(root, 24).unwrap(),
            RetentionPlan::Probe(_)
        ));
        store
            .conn
            .execute(
                "UPDATE sessions SET parent_id=NULL WHERE id=?1",
                [grandchild.to_string()],
            )
            .unwrap();
        assert_ne!(
            finished,
            store.retention_input_fingerprint(root, 24).unwrap()
        );
        assert_eq!(store.retention_archive_one(root, 24).unwrap(), None);
    }
}
