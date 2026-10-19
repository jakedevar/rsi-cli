//! #2682: who owns a sandbox directory's `target/`, and the disk-runaway notice.
//!
//! The terminal-sandbox reclaim sweep only visits sessions that hold a live
//! custody root. Two kinds of build output sat outside it while the hub disk
//! reached 98%: directories no session row names at all (orphans), and
//! directories only finished sessions without a live custody root name (a
//! failed or quarantined cleanup, a purged row whose directory survived).
//! [`Store::stray_target_owner`] classifies one directory so the daemon can
//! reclaim only its `target/`, never the source worktree.
//!
//! [`Store::record_runaway_disk_notice`] tells the owning manager that one
//! running worker's `target/` passed the per-sandbox size threshold, the way
//! `runaway_process` does for CPU time (#1337). It never stops the worker.
//!
//! The size judged is `target/` without `target/scoped-baselines` (#2782): that
//! directory is the transient baseline build of the worker's own `scoped_test`
//! run, removed when the run settles, so it is reported apart and never counts
//! toward the threshold. While the owner runs a `scoped_test` job the record
//! carries no halt suggestion, since a halt would throw the acceptance run away.

use super::Store;
use super::runaway_process::{RunawayNoticeRoute, RunawayRecord};
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use uuid::Uuid;

/// `record_kind` of the disk notice and the subject prefix.
pub const RUNAWAY_DISK_NOTICE: &str = "runaway_disk";

/// One running worker's `target/` may grow to this before its manager is told
/// (#2682, #121). A fixed ceiling, not an operator setting: the notice never
/// acts, and the hub's disk watermarks are the operator's lever.
pub const RUNAWAY_DISK_THRESHOLD_BYTES: u64 = 40 * 1024 * 1024 * 1024;

/// Who, if anyone, still owns a sandbox directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrayTargetOwner {
    /// A session that has not finished, or a live custody root, or a finished
    /// session with `Live` cleanup state names it. The ordinary sweep (or the
    /// owner) handles it; it is not stray.
    Claimed,
    /// No session row and no custody root names the directory.
    Orphan,
    /// Only finished sessions without live custody name it.
    Detached {
        /// The newest finished session that named the root, when a session
        /// row still carries it.
        session_id: Option<Uuid>,
    },
}

/// One running worker whose `target/` passed [`RUNAWAY_DISK_THRESHOLD_BYTES`].
#[derive(Debug, Clone, PartialEq)]
pub struct RunawayDiskReport {
    pub session_id: Uuid,
    /// The sandbox directory name under the sandbox base (never a path).
    pub sandbox: String,
    /// Allocated bytes of `target/` seen without the transient
    /// `scoped-baselines` tree (a lower bound when `truncated`).
    pub target_bytes: u64,
    /// Allocated bytes of `target/scoped-baselines`, the worker's own
    /// `scoped_test` baseline build; counted apart from `target_bytes`.
    pub baseline_bytes: u64,
    pub truncated: bool,
    pub threshold_bytes: u64,
    /// The owner has a queued or running `scoped_test` job (the supported
    /// acceptance path), so its build output is transient.
    pub scoped_test_live: bool,
    /// `target/debug/deps` holds test binaries of `rsid` built from more than
    /// one feature set (a per-shard feature build), the one cause the
    /// `cargo clean` between shards advice fits.
    pub shard_copies: bool,
    pub observed_at: DateTime<Utc>,
}

impl RunawayDiskReport {
    /// A notice repeats only each time the size passes another whole multiple
    /// of the threshold, so a stuck 45 GB target is told once.
    #[must_use]
    pub fn multiple(&self) -> u64 {
        self.target_bytes / self.threshold_bytes.max(1)
    }

    /// The one-call next step the record suggests. It depends on the cause: a
    /// live `scoped_test` job gets no halt (the build output is transient and
    /// halting would discard the acceptance run, #2782), and the per-shard
    /// advice is given only when several feature-set copies are present.
    #[must_use]
    pub fn suggested_action(&self) -> String {
        if self.scoped_test_live {
            return "No action now: the worker's own scoped_test job is running and its build output is transient (the baseline build under target/scoped-baselines is removed when the run settles). Look again after the job settles; act only if free disk is low.".to_owned();
        }
        let advice = if self.shard_copies {
            "builds into a shared or smaller target (a per-shard feature build leaves its own copy of the daemon; clear `target/` with `cargo clean` between shards)"
        } else {
            "builds into a shared or smaller target"
        };
        format!(
            "AgentHalt {{\"session_id\":\"{}\"}}, then AgentContinueChild with a scoped instruction that {advice}",
            self.session_id
        )
    }

    /// The typed record carried by the notice or mail.
    #[must_use]
    pub fn state(&self) -> serde_json::Value {
        json!({
            "record_kind": RUNAWAY_DISK_NOTICE,
            "session_id": self.session_id,
            "sandbox": self.sandbox,
            "target_gib": (self.target_bytes as f64 / (1u64 << 30) as f64 * 10.0).round() / 10.0,
            "target_bytes_is_lower_bound": self.truncated,
            "scoped_baseline_gib": (self.baseline_bytes as f64 / (1u64 << 30) as f64 * 10.0).round() / 10.0,
            "scoped_test_live": self.scoped_test_live,
            "shard_copies": self.shard_copies,
            "threshold_gib": self.threshold_bytes / (1 << 30),
            "observed_at": self.observed_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
            "suggested": self.suggested_action(),
        })
    }
}

// A row names the directory by its exact stored path (?1) or, when the base
// directory is spelled differently (a symlinked home), by the directory name
// (?2) as the last path component. Reclaiming a live worker's `target/` because
// two spellings of one path differ would be the worse mistake.
const CLAIMED_SQL: &str = "SELECT
    EXISTS(SELECT 1 FROM sessions
           WHERE (sandbox_root=?1 OR substr(sandbox_root,-length(?2)-1)='/'||?2)
             AND (status NOT IN ('Completed','Failed','Interrupted','Archived','Deleted')
                  OR COALESCE(sandbox_cleanup_state,'')='Live'))
    OR EXISTS(SELECT 1 FROM sandbox_custody_roots
              WHERE (sandbox_root=?1 OR substr(sandbox_root,-length(?2)-1)='/'||?2)
                AND state='live')";

const NEWEST_NAMING_SESSION_SQL: &str = "SELECT id FROM sessions
     WHERE (sandbox_root=?1 OR substr(sandbox_root,-length(?2)-1)='/'||?2)
     ORDER BY updated_at DESC,id DESC LIMIT 1";

const ANY_ROOT_SQL: &str = "SELECT EXISTS(SELECT 1 FROM sandbox_custody_roots
     WHERE (sandbox_root=?1 OR substr(sandbox_root,-length(?2)-1)='/'||?2))";

// A worker that parks on `AgentSubmitJob` + `when.jobs_terminal` ends its provider
// turn, so its session reads `Completed` while the job that grows the target runs.
// An owner of a queued or running build/test job is therefore a running owner too
// (#2696). Every session naming the root is considered, so a continuation that
// shares the sandbox counts whichever session of the lineage owns the job.
const RUNNING_OWNER_SQL: &str = "SELECT s.id FROM sessions s
     WHERE (s.sandbox_root=?1 OR substr(s.sandbox_root,-length(?2)-1)='/'||?2)
       AND COALESCE(s.sandbox_cleanup_state,'')='Live'
       AND (s.status IN ('Starting','Running','WaitingApproval')
            OR EXISTS(SELECT 1 FROM agent_jobs j
                      WHERE j.owner_session_id=s.id
                        AND j.state IN ('queued','running')
                        AND j.kind IN ('build','test')))
     ORDER BY (s.status IN ('Starting','Running','WaitingApproval')) DESC,
              s.updated_at DESC,s.id DESC LIMIT 1";

// A queued or running `scoped_test` job of any session naming the root (#2782).
const LIVE_SCOPED_TEST_SQL: &str = "SELECT EXISTS(SELECT 1 FROM agent_jobs j
     JOIN sessions s ON s.id=j.owner_session_id
     WHERE (s.sandbox_root=?1 OR substr(s.sandbox_root,-length(?2)-1)='/'||?2)
       AND j.state IN ('queued','running')
       AND j.kind='test'
       AND json_extract(j.params_json,'$.scoped_test') IS NOT NULL)";

/// The last path component, the only part a different spelling of the sandbox
/// base cannot change. An empty or separator-only name matches nothing.
fn directory_name(sandbox_root: &str) -> String {
    std::path::Path::new(sandbox_root)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .unwrap_or_else(|| "\u{0}no-directory-name".to_owned())
}

impl Store {
    /// Classify the sandbox directory `sandbox_root` (the exact string the
    /// allocator stores) for stray `target/` reclaim.
    ///
    /// # Errors
    /// A persistence error.
    pub fn stray_target_owner(&self, sandbox_root: &str) -> Result<StrayTargetOwner> {
        let name = directory_name(sandbox_root);
        let claimed: bool =
            self.conn
                .query_row(CLAIMED_SQL, params![sandbox_root, name], |row| row.get(0))?;
        if claimed {
            return Ok(StrayTargetOwner::Claimed);
        }
        let session = self
            .conn
            .query_row(
                NEWEST_NAMING_SESSION_SQL,
                params![sandbox_root, name],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|id| {
                Uuid::parse_str(&id)
                    .map_err(|_| DaemonError::Store("stored session id is invalid".into()))
            })
            .transpose()?;
        if session.is_some() {
            return Ok(StrayTargetOwner::Detached {
                session_id: session,
            });
        }
        let any_root: bool =
            self.conn
                .query_row(ANY_ROOT_SQL, params![sandbox_root, name], |row| row.get(0))?;
        Ok(if any_root {
            StrayTargetOwner::Detached { session_id: None }
        } else {
            StrayTargetOwner::Orphan
        })
    }

    /// The session that owns the live sandbox at `sandbox_root` and is not
    /// finished, when any: whom to tell about a runaway `target/`. A session
    /// whose turn ended but that owns a queued or running build/test job is
    /// still working (#2696). A turn in progress is preferred.
    ///
    /// # Errors
    /// A persistence error.
    pub fn running_sandbox_owner(&self, sandbox_root: &str) -> Result<Option<Uuid>> {
        let name = directory_name(sandbox_root);
        self.conn
            .query_row(RUNNING_OWNER_SQL, params![sandbox_root, name], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
            .map(|id| {
                Uuid::parse_str(&id)
                    .map_err(|_| DaemonError::Store("stored session id is invalid".into()))
            })
            .transpose()
    }

    /// Whether a session naming the sandbox at `sandbox_root` owns a queued or
    /// running `scoped_test` job (#2782): its `target/` growth, the baseline
    /// build included, is the supported acceptance path, not a runaway.
    ///
    /// # Errors
    /// A persistence error.
    pub fn sandbox_has_live_scoped_test(&self, sandbox_root: &str) -> Result<bool> {
        let name = directory_name(sandbox_root);
        Ok(self
            .conn
            .query_row(LIVE_SCOPED_TEST_SQL, params![sandbox_root, name], |row| {
                row.get(0)
            })?)
    }

    /// Route one runaway-`target/` record to the owning manager (see the
    /// module docs). Idempotent per session and threshold multiple.
    ///
    /// # Errors
    /// A persistence error; the caller treats the notice as best-effort.
    pub fn record_runaway_disk_notice(
        &self,
        report: &RunawayDiskReport,
    ) -> Result<RunawayNoticeRoute> {
        let multiple = report.multiple();
        self.route_runaway_record(
            report.session_id,
            &RunawayRecord {
                kind: RUNAWAY_DISK_NOTICE,
                version: format!("target-{multiple}x"),
                state: report.state(),
                headline: report.state().to_string(),
                suggested: report.suggested_action(),
                // A notice made under a live scoped_test job carries no halt, so
                // a later one for the same multiple must still be able to.
                idempotency_key: format!(
                    "runaway-disk:{}:{multiple}{}",
                    report.session_id,
                    if report.scoped_test_live { ":job" } else { "" }
                ),
                observed_at: report.observed_at,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::agent_jobs::NewAgentJob;
    use crate::store::tests::make_test_session;
    use rsi_common::agent_jobs::{BuildCommand, BuildJobParams, JobParams, JobState, JobWake};
    use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionStatus};

    fn seed(
        store: &Store,
        id: u128,
        root: &str,
        status: SessionStatus,
        cleanup: SandboxCleanupState,
    ) -> Uuid {
        seed_in_project(store, id, root, status, cleanup, None)
    }

    fn seed_in_project(
        store: &Store,
        id: u128,
        root: &str,
        status: SessionStatus,
        cleanup: SandboxCleanupState,
        project: Option<Uuid>,
    ) -> Uuid {
        let mut session = make_test_session();
        session.project_id = project;
        session.id = Uuid::from_u128(id);
        session.status = status;
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(root.into());
        session.sandbox_branch = Some(format!("rsi/stray-{id}"));
        session.sandbox_cleanup_state = Some(cleanup);
        store.insert_session(&session).unwrap();
        session.id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_directory_no_row_names_is_an_orphan() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(
            store.stray_target_owner("/sandboxes/none").unwrap(),
            StrayTargetOwner::Orphan
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn finished_sessions_without_live_cleanup_leave_a_detached_directory() {
        let store = Store::open_in_memory().unwrap();
        let archived = seed(
            &store,
            1,
            "/sandboxes/a",
            SessionStatus::Archived,
            SandboxCleanupState::Failed,
        );
        assert_eq!(
            store.stray_target_owner("/sandboxes/a").unwrap(),
            StrayTargetOwner::Detached {
                session_id: Some(archived)
            }
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_running_or_live_session_keeps_its_directory_claimed() {
        let store = Store::open_in_memory().unwrap();
        seed(
            &store,
            2,
            "/sandboxes/running",
            SessionStatus::Running,
            SandboxCleanupState::Live,
        );
        seed(
            &store,
            3,
            "/sandboxes/completed-live",
            SessionStatus::Completed,
            SandboxCleanupState::Live,
        );
        // A finished session cannot release a root a running successor shares.
        seed(
            &store,
            4,
            "/sandboxes/shared",
            SessionStatus::Archived,
            SandboxCleanupState::Failed,
        );
        seed(
            &store,
            5,
            "/sandboxes/shared",
            SessionStatus::Starting,
            SandboxCleanupState::Live,
        );
        for root in [
            "/sandboxes/running",
            "/sandboxes/completed-live",
            "/sandboxes/shared",
        ] {
            assert_eq!(
                store.stray_target_owner(root).unwrap(),
                StrayTargetOwner::Claimed,
                "{root}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_running_owner_is_the_unfinished_live_session() {
        let store = Store::open_in_memory().unwrap();
        let running = seed(
            &store,
            6,
            "/sandboxes/w",
            SessionStatus::Running,
            SandboxCleanupState::Live,
        );
        seed(
            &store,
            7,
            "/sandboxes/done",
            SessionStatus::Completed,
            SandboxCleanupState::Live,
        );
        assert_eq!(
            store.running_sandbox_owner("/sandboxes/w").unwrap(),
            Some(running)
        );
        assert_eq!(
            store.running_sandbox_owner("/sandboxes/done").unwrap(),
            None
        );
        assert_eq!(
            store.running_sandbox_owner("/sandboxes/none").unwrap(),
            None
        );
    }

    fn insert_job(store: &Store, owner: Uuid, kind: &str, state: JobState) -> Uuid {
        let id = Uuid::new_v4();
        let params = match kind {
            "test" => serde_json::from_value(
                json!({"kind":"test","scoped_test":{"base":"origin/rolling"}}),
            )
            .unwrap(),
            _ => JobParams::Build(BuildJobParams {
                command: BuildCommand::Check,
                package: None,
                workspace: true,
                all_targets: false,
                release: false,
                no_default_features: false,
                features: Vec::new(),
                shards: Vec::new(),
                cpu_quota_percent: None,
                sha: None,
            }),
        };
        let new = NewAgentJob {
            id,
            owner_session_id: owner,
            project_id: None,
            name: None,
            params,
            cwd: "/tmp/sandbox".into(),
            unit_name: format!("rsi-job-{id}"),
            log_path: format!("/tmp/jobs/{id}.log"),
            status_path: format!("/tmp/jobs/{id}.status"),
            idempotency_key: None,
            wake: JobWake::None,
        };
        store
            .insert_agent_job_in_state(&new, Utc::now(), state)
            .unwrap();
        id
    }

    /// #2696: the required `AgentSubmitJob` + `when.jobs_terminal` workflow leaves the
    /// owning session `Completed` while its job grows the target.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_completed_owner_of_a_live_build_or_test_job_is_a_running_owner() {
        let store = Store::open_in_memory().unwrap();
        let parked = seed(
            &store,
            20,
            "/sandboxes/parked",
            SessionStatus::Completed,
            SandboxCleanupState::Live,
        );
        assert_eq!(
            store.running_sandbox_owner("/sandboxes/parked").unwrap(),
            None,
            "a completed session with no job is finished"
        );
        let job = insert_job(&store, parked, "test", JobState::Running);
        assert_eq!(
            store.running_sandbox_owner("/sandboxes/parked").unwrap(),
            Some(parked)
        );
        store
            .settle_agent_job(
                job,
                JobState::Succeeded,
                &Default::default(),
                false,
                Utc::now(),
            )
            .unwrap();
        assert_eq!(
            store.running_sandbox_owner("/sandboxes/parked").unwrap(),
            None,
            "a settled job no longer keeps its owner running"
        );
        insert_job(&store, parked, "build", JobState::Queued);
        assert_eq!(
            store.running_sandbox_owner("/sandboxes/parked").unwrap(),
            Some(parked),
            "a queued build job counts"
        );
        // A job owned by a continuation sharing the root counts for the root.
        seed(
            &store,
            21,
            "/sandboxes/lineage",
            SessionStatus::Archived,
            SandboxCleanupState::Failed,
        );
        let successor = seed(
            &store,
            22,
            "/sandboxes/lineage",
            SessionStatus::Completed,
            SandboxCleanupState::Live,
        );
        insert_job(&store, successor, "build", JobState::Running);
        assert_eq!(
            store.running_sandbox_owner("/sandboxes/lineage").unwrap(),
            Some(successor)
        );
        // A job of another kind, or of another session, does not.
        let other = seed(
            &store,
            23,
            "/sandboxes/other",
            SessionStatus::Completed,
            SandboxCleanupState::Live,
        );
        insert_job(&store, Uuid::new_v4(), "test", JobState::Running);
        assert_eq!(
            store.running_sandbox_owner("/sandboxes/other").unwrap(),
            None
        );
        let _ = other;
    }

    /// #2696: a completed owner with a running job and an oversized target routes a
    /// `runaway_disk` notice to its project's manager; a Running owner still does.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_completed_owner_with_a_running_job_and_a_huge_target_routes_a_notice() {
        let store = Store::open_in_memory().unwrap();
        let stamp = Utc::now();
        let project = Uuid::new_v4();
        store
            .insert_project(&rsi_common::types::Project {
                id: project,
                name: "stray".into(),
                path: None,
                description: None,
                color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: stamp,
                updated_at: stamp,
            })
            .unwrap();
        let mut seat = make_test_session();
        seat.id = Uuid::from_u128(90);
        seat.status = SessionStatus::Running;
        seat.project_id = Some(project);
        store.insert_session(&seat).unwrap();
        store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: project,
                    session_id: seat.id,
                    epic_ids: None,
                    group_ids: Vec::new(),
                    expected_row_version: 0,
                },
            )
            .unwrap();

        let parked = seed_in_project(
            &store,
            30,
            "/sandboxes/huge",
            SessionStatus::Completed,
            SandboxCleanupState::Live,
            Some(project),
        );
        let running = seed_in_project(
            &store,
            31,
            "/sandboxes/live",
            SessionStatus::Running,
            SandboxCleanupState::Live,
            Some(project),
        );
        insert_job(&store, parked, "test", JobState::Running);
        for (root, expected) in [("/sandboxes/huge", parked), ("/sandboxes/live", running)] {
            let owner = store.running_sandbox_owner(root).unwrap();
            assert_eq!(owner, Some(expected), "{root}");
            let report = disk_report(expected, 2 * RUNAWAY_DISK_THRESHOLD_BYTES);
            assert!(report.target_bytes > report.threshold_bytes);
            assert!(
                matches!(
                    store.record_runaway_disk_notice(&report).unwrap(),
                    RunawayNoticeRoute::ManagerNotice(_)
                ),
                "{root}"
            );
        }
    }

    fn disk_report(session: Uuid, bytes: u64) -> RunawayDiskReport {
        RunawayDiskReport {
            session_id: session,
            sandbox: "e97956b8".into(),
            target_bytes: bytes,
            baseline_bytes: 0,
            truncated: false,
            threshold_bytes: RUNAWAY_DISK_THRESHOLD_BYTES,
            scoped_test_live: false,
            shard_copies: false,
            observed_at: Utc::now(),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn runaway_disk_record_names_the_session_size_and_a_halt() {
        let session = Uuid::from_u128(42);
        let report = disk_report(session, 96 * (1 << 30));
        let state = report.state();
        assert_eq!(state["record_kind"], RUNAWAY_DISK_NOTICE);
        assert_eq!(state["session_id"], session.to_string());
        assert_eq!(state["target_gib"], 96.0);
        assert_eq!(state["threshold_gib"], 40);
        assert!(
            state["suggested"]
                .as_str()
                .unwrap()
                .starts_with(&format!("AgentHalt {{\"session_id\":\"{session}\"}}"))
        );
        // One notice per whole multiple of the threshold.
        assert_eq!(disk_report(session, 45 * (1 << 30)).multiple(), 1);
        assert_eq!(report.multiple(), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_suggestion_follows_the_cause_of_the_growth() {
        let session = Uuid::from_u128(43);
        let mut report = disk_report(session, 45 * (1 << 30));
        let plain = report.suggested_action();
        assert!(plain.starts_with("AgentHalt"), "{plain}");
        assert!(!plain.contains("per-shard"), "{plain}");
        report.shard_copies = true;
        let sharded = report.suggested_action();
        assert!(sharded.starts_with("AgentHalt"), "{sharded}");
        assert!(sharded.contains("cargo clean"), "{sharded}");
        report.scoped_test_live = true;
        report.baseline_bytes = 6 << 30;
        let live = report.suggested_action();
        assert!(!live.contains("AgentHalt"), "{live}");
        assert!(live.contains("scoped-baselines"), "{live}");
        let state = report.state();
        assert_eq!(state["scoped_baseline_gib"], 6.0);
        assert_eq!(state["scoped_test_live"], true);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_live_scoped_test_job_is_seen_through_any_session_naming_the_sandbox() {
        let store = Store::open_in_memory().unwrap();
        let owner = seed(
            &store,
            60,
            "/sandboxes/accepting",
            SessionStatus::Completed,
            SandboxCleanupState::Live,
        );
        seed(
            &store,
            61,
            "/sandboxes/building",
            SessionStatus::Running,
            SandboxCleanupState::Live,
        );
        assert!(
            !store
                .sandbox_has_live_scoped_test("/sandboxes/accepting")
                .unwrap()
        );
        insert_job(&store, owner, "build", JobState::Running);
        assert!(
            !store
                .sandbox_has_live_scoped_test("/sandboxes/accepting")
                .unwrap()
        );
        let job = insert_job(&store, owner, "test", JobState::Running);
        assert!(
            store
                .sandbox_has_live_scoped_test("/sandboxes/accepting")
                .unwrap()
        );
        assert!(
            !store
                .sandbox_has_live_scoped_test("/sandboxes/building")
                .unwrap()
        );
        store
            .settle_agent_job(
                job,
                JobState::Succeeded,
                &Default::default(),
                false,
                Utc::now(),
            )
            .unwrap();
        assert!(
            !store
                .sandbox_has_live_scoped_test("/sandboxes/accepting")
                .unwrap()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn runaway_disk_notice_for_an_unknown_session_is_unrouted() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(
            store
                .record_runaway_disk_notice(&disk_report(Uuid::new_v4(), 50 * (1 << 30)))
                .unwrap(),
            RunawayNoticeRoute::Unrouted
        );
    }
}
