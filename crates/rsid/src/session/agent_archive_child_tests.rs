//! #670 R2 S2: `AgentArchiveChild` store and session-layer tests (plan §7).
//!
//! RPC dispatch and catalog pins are deferred to the rpc.rs wiring step.
// Each test owns its `Fx` (TempDir, database, manager) for its whole body on
// purpose: dropping the fixture early would delete the database under it.
#![allow(clippy::significant_drop_tightening)]

use super::SessionManager;
use super::agent_verbs::archive_child_test_seam;
use super::agent_verbs::tests::test_session;
use super::types::CompletedSession;
use crate::bus::DaemonEvent;
use crate::error::DaemonError;
use crate::store::Store;
use crate::store::daemon_settings::{AutofileCause, c5_autofile_pending_key};
use rsi_common::agent_coordination::{
    AgentArchiveChildRequestV1, AgentArchiveChildResultV1, AgentArchiveErrorCodeV1 as Code,
    AgentArchiveErrorV1, AgentArchiveRefusalDetailV1 as Detail,
};
use rsi_common::types::{
    ConversationEvent, EventType, Role, SandboxCleanupState, SandboxKind, Session, SessionKind,
    SessionStatus,
};
use rusqlite::params;
use rusqlite::types::Value;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

/// Owned, `Send` SQL values for fixture statements run across an `.await`.
macro_rules! vals {
    ($($value:expr),* $(,)?) => { vec![$(Value::from($value)),*] };
}

const HUMAN_GATE: &str = "orchestration_outcome_v1: {\"schema_version\":1,\"mode\":\"program\",\"next_slice_ready\":false,\"continuation_state\":\"human_gate\",\"blocker_class\":\"production\",\"evidence\":\"operator approval is required\"}";
const QUEUE_EXHAUSTED: &str = "orchestration_outcome_v1: {\"schema_version\":1,\"mode\":\"program\",\"next_slice_ready\":false,\"continuation_state\":\"queue_exhausted\",\"evidence\":\"the program queue is exhausted\"}";

struct Fx {
    manager: SessionManager,
    dir: TempDir,
    epic: Uuid,
    lead: Uuid,
    child: Uuid,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn digest(seed: char) -> String {
    format!("sha256:{}", seed.to_string().repeat(64))
}

impl Fx {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let sandbox = dir.path().join("sandboxes");
        std::fs::create_dir(&sandbox).unwrap();
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(64)),
            Store::open(&dir.path().join("rsi.db")).unwrap(),
            false,
            dir.path().join("daemon.sock"),
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
            sandbox,
        )
        .unwrap();
        let (epic, lead, child) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let fx = Self {
            manager,
            dir,
            epic,
            lead,
            child,
        };
        let mut epic_row = fx.session(epic, None, SessionStatus::Running);
        epic_row.session_kind = SessionKind::Epic;
        epic_row.lead_session_id = Some(lead);
        fx.insert(epic_row).await;
        fx.insert(fx.session(lead, Some(epic), SessionStatus::Running))
            .await;
        fx.insert(fx.session(child, Some(epic), SessionStatus::Failed))
            .await;
        fx
    }

    fn session(&self, id: Uuid, parent: Option<Uuid>, status: SessionStatus) -> Session {
        let mut row = test_session(id, self.dir.path().to_path_buf());
        row.parent_id = parent;
        row.status = status;
        row
    }

    async fn insert(&self, row: Session) {
        self.manager
            .store
            .lock()
            .await
            .insert_session(&row)
            .unwrap();
        if matches!(
            row.status,
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted
        ) {
            self.manager
                .completed
                .write()
                .await
                .insert(row.id, CompletedSession::for_test(row));
        }
    }

    async fn sql(&self, sql: &str, values: Vec<Value>) {
        self.manager
            .store
            .lock()
            .await
            .conn
            .execute(sql, rusqlite::params_from_iter(values))
            .unwrap();
    }

    /// Insert a fixture row whose foreign references (reviewers, manager
    /// operations, invocations, jobs) are synthetic and have no rows.
    async fn sql_synthetic_refs(&self, sql: &str, values: Vec<Value>) {
        let store = self.manager.store.lock().await;
        store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        let result = store.conn.execute(sql, rusqlite::params_from_iter(values));
        store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        drop(store);
        result.unwrap();
    }

    async fn request(&self, target: Uuid) -> AgentArchiveChildRequestV1 {
        let cursor = self
            .manager
            .store
            .lock()
            .await
            .agent_continuation_cursor(target)
            .unwrap();
        AgentArchiveChildRequestV1 {
            target_session_id: target,
            expected_tip_session_id: cursor.tip_session_id,
            expected_event_sequence: cursor.event_sequence,
            expected_custody_generation: cursor.custody_generation,
        }
    }

    async fn archive(
        &self,
        caller: Uuid,
        target: Uuid,
    ) -> crate::error::Result<AgentArchiveChildResultV1> {
        let request = self.request(target).await;
        self.manager.agent_archive_child(caller, request).await
    }

    async fn status(&self, id: Uuid) -> SessionStatus {
        self.manager
            .store
            .lock()
            .await
            .get_session(id)
            .unwrap()
            .unwrap()
            .status
    }

    async fn c5_marker(&self, id: Uuid) -> bool {
        self.manager
            .store
            .lock()
            .await
            .get_c5_autofile_pending(&c5_autofile_pending_key(id))
            .unwrap()
            .is_some()
    }

    async fn stage_c5(&self, id: Uuid) {
        self.manager
            .store
            .lock()
            .await
            .update_failed_and_stage_c5_autofile(id, AutofileCause::NonZeroExit)
            .unwrap();
        assert!(self.c5_marker(id).await, "fixture must stage the C5 marker");
    }

    /// A durable launched spawn reservation `owner -> child` under the Epic.
    async fn spawn_request(&self, owner: Uuid, child: Uuid, ordinal: i64) {
        let at = now();
        self.sql(
            "INSERT INTO agent_spawn_requests (
                 spawn_request_id, owner_session_id, idempotency_digest, request_fingerprint,
                 request_json, child_session_id, epic_id, kind, state, reserved_at, updated_at,
                 launched_at, epic_spawn_ordinal)
             VALUES (?1,?2,?3,?4,'{}',?5,?6,'Task','launched',?7,?7,?7,?8)",
            vals![
                Uuid::new_v4().to_string(),
                owner.to_string(),
                digest('a'),
                digest('b'),
                child.to_string(),
                self.epic.to_string(),
                at,
                ordinal,
            ],
        )
        .await;
    }

    async fn job(
        &self,
        mode: &str,
        origin: Uuid,
        watch: Option<Uuid>,
    ) -> rsi_common::types::ScheduledJob {
        use crate::session::harness::tools::schedule_wake::{
            ScheduleWakeRequest, build_agent_scheduled_job,
        };
        let job = build_agent_scheduled_job(ScheduleWakeRequest {
            message: "archive fixture wake".into(),
            in_seconds: (mode != "program_guard").then_some(60),
            at: None,
            name: None,
            every_seconds: None,
            mode: Some(mode.into()),
            working_dir: self.dir.path().to_path_buf(),
            provider: None,
            model: None,
            project_id: Some(crate::store::d04_test_project_id()),
            origin_session_id: Some(origin),
            watch_session_id: watch,
        })
        .unwrap();
        self.manager
            .store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .unwrap();
        job
    }

    async fn job_enabled(&self, id: Uuid) -> bool {
        self.manager
            .store
            .lock()
            .await
            .get_scheduled_job(&id)
            .unwrap()
            .unwrap()
            .enabled
    }

    async fn assistant_output(&self, id: Uuid, content: &str) {
        self.manager
            .store
            .lock()
            .await
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: id,
                sequence: 1,
                event_type: EventType::System,
                role: Some(Role::Assistant),
                content: content.into(),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .unwrap();
    }

    async fn review_assignment(&self, author: Uuid, state: &str, work_key: &str) -> Uuid {
        let assignment = Uuid::new_v4();
        let active = state == "active";
        let at = now();
        self.sql_synthetic_refs(
            "INSERT INTO manager_review_assignments (
                 assignment_id, project_id, epic_id, manager_session_id, scope_version, work_key,
                 spec_revision, author_session_id, source_sha, reviewer_session_id,
                 reviewer_invocation_id, reviewer_custody_id, reviewer_custody_generation,
                 action_operation_id, state, row_version, request_json, request_fingerprint,
                 created_at, updated_at)
             VALUES (?1,?2,?3,?4,1,?5,1,?6,?7,?8,?9,?10,?11,?12,?13,1,'{}',?14,?15,?15)",
            vals![
                assignment.to_string(),
                crate::store::d04_test_project_id().to_string(),
                self.epic.to_string(),
                self.lead.to_string(),
                work_key.to_string(),
                author.to_string(),
                "a".repeat(40),
                active.then(|| Uuid::new_v4().to_string()),
                active.then(|| Uuid::new_v4().to_string()),
                active.then(|| Uuid::new_v4().to_string()),
                active.then_some(1_i64),
                active.then(|| Uuid::new_v4().to_string()),
                state.to_string(),
                digest('c'),
                at,
            ],
        )
        .await;
        assignment
    }

    async fn review_work_fact(&self, source: Uuid, work_key: &str) {
        let payload = serde_json::json!({
            "key": work_key,
            "source_session_id": source.to_string(),
            "source_commit": "a".repeat(40),
            "spec_revision": 1,
            "integration": null,
            "required_gates": ["implementation", "review"],
        });
        self.sql_synthetic_refs(
            "INSERT INTO harness_manager_v2_work_facts (
                 project_id, kind, record_key, epic_id, work_key, row_version, payload_json,
                 archived, manager_session_id, scope_version, policy_version, created_at, updated_at)
             VALUES (?1,'work',?2,?3,?2,1,?4,0,?5,1,1,?6,?6)",
            vals![
                crate::store::d04_test_project_id().to_string(),
                work_key.to_string(),
                self.epic.to_string(),
                payload.to_string(),
                self.lead.to_string(),
                now(),
            ],
        )
        .await;
    }

    async fn hand_lead_to(&self, new_lead: Uuid) {
        self.sql(
            "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
            vals![self.epic.to_string(), new_lead.to_string()],
        )
        .await;
    }

    /// Every column of one row, for "row unchanged" assertions.
    async fn row_snapshot(&self, table: &str, key: &str, id: Uuid) -> Vec<Value> {
        let store = self.manager.store.lock().await;
        let mut statement = store
            .conn
            .prepare(&format!("SELECT * FROM {table} WHERE {key}=?1"))
            .unwrap();
        let columns = statement.column_count();
        statement
            .query_row([id.to_string()], |row| {
                (0..columns)
                    .map(|index| row.get::<_, Value>(index))
                    .collect()
            })
            .unwrap()
    }

    /// Queue lead-to-`target` mail and leave it in the pending `state`.
    ///
    /// `queued` is inserted exactly as the mailbox accepts it. The later
    /// pending states are otherwise reachable only through a full delivery
    /// attempt world (attempt, transition and invocation rows) owned by the
    /// mailbox's own tests, so the mailbox state-machine triggers are dropped
    /// for one UPDATE on this test database and recreated verbatim.
    async fn pending_mail(&self, target: Uuid, state: &str) -> Uuid {
        let id = Uuid::new_v4();
        let at = now();
        self.sql(
            "INSERT INTO agent_messages (
                 id, owner_session_id, target_session_id, target_spawn_request_id,
                 idempotency_digest, request_fingerprint, payload_digest, payload,
                 created_at, state, state_version, attempt_count, updated_at)
             VALUES (?1,?2,?3,NULL,?4,?5,?6,'finish the handoff',?7,'queued',0,0,?7)",
            vals![
                id.to_string(),
                self.lead.to_string(),
                target.to_string(),
                format!("sha256:{}{}", id.simple(), id.simple()),
                digest('e'),
                digest('f'),
                at,
            ],
        )
        .await;
        if state == "queued" {
            return id;
        }
        let store = self.manager.store.lock().await;
        let triggers: Vec<(String, String)> = store
            .conn
            .prepare(
                "SELECT name, sql FROM sqlite_master WHERE type='trigger'
                  AND tbl_name='agent_messages' AND name IN (
                      'agent_messages_v81_state_forward', 'agent_messages_v81_cas_coherence',
                      'agent_messages_v81_requeue_requires_no_effect')",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(triggers.len(), 3, "mailbox state-machine triggers present");
        for (name, _) in &triggers {
            store
                .conn
                .execute_batch(&format!("DROP TRIGGER {name}"))
                .unwrap();
        }
        store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        let updated = store.conn.execute(
            "UPDATE agent_messages
                SET state=?2, state_version=1, attempt_count=1, current_attempt_number=1,
                    uncertain_at=CASE WHEN ?2='uncertain' THEN ?3 END, updated_at=?3
              WHERE id=?1",
            params![id.to_string(), state, now()],
        );
        store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        for (_, sql) in &triggers {
            store.conn.execute_batch(sql).unwrap();
        }
        drop(store);
        assert_eq!(updated.unwrap(), 1);
        id
    }

    /// A live successor reservation naming `predecessor`, in `state`.
    async fn successor_reservation(&self, predecessor: Uuid, state: &str) -> Uuid {
        let reservation = Uuid::new_v4();
        let launched = state != "reserved";
        let uncertain = state == "uncertain";
        let at = now();
        self.sql_synthetic_refs(
            "INSERT INTO agent_successor_reservations (
                 reservation_id, predecessor_session_id, epic_id, candidate_session_id,
                 caller_key_digest, request_json, request_fingerprint, candidate_kind,
                 inherited_launch_json, expected_lead_session_id, expected_lead_generation,
                 state, state_version, launch_attempt_id, model_invocation_id,
                 terminal_reason, safe_error_class, reserved_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,'{}',?6,'Task','{}',?2,1,?7,1,?8,?9,?10,?10,?11,?11)",
            vals![
                reservation.to_string(),
                predecessor.to_string(),
                self.epic.to_string(),
                Uuid::new_v4().to_string(),
                format!("sha256:{}", "1".repeat(64)),
                format!("sha256:{}", "2".repeat(64)),
                state.to_string(),
                launched.then(|| Uuid::new_v4().to_string()),
                launched.then(|| Uuid::new_v4().to_string()),
                uncertain.then(|| "provider_outcome_unknown".to_string()),
                at,
            ],
        )
        .await;
        reservation
    }

    fn archived_events(&self) -> tokio::sync::broadcast::Receiver<Arc<DaemonEvent>> {
        self.manager.event_bus.subscribe()
    }
}

fn count_archived(rx: &mut tokio::sync::broadcast::Receiver<Arc<DaemonEvent>>, id: Uuid) -> usize {
    let mut count = 0;
    while let Ok(event) = rx.try_recv() {
        if matches!(*event, DaemonEvent::SessionArchived { session_id, .. } if session_id == id) {
            count += 1;
        }
    }
    count
}

fn refusal(error: &DaemonError) -> AgentArchiveErrorV1 {
    let DaemonError::StructuredRpc { data, .. } = error else {
        panic!("AgentArchiveChild refusals must be structured: {error:?}");
    };
    serde_json::from_value(data.clone()).expect("typed archive error envelope")
}

fn assert_refused(
    result: crate::error::Result<AgentArchiveChildResultV1>,
    code: Code,
    detail: Option<Detail>,
) {
    let error = result.expect_err("archive must be refused");
    let envelope = refusal(&error);
    assert_eq!(
        (envelope.code, envelope.detail),
        (code, detail),
        "{error:?}"
    );
}

// --- Logical archive, sandbox retention, C5 marker ------------------------

#[tokio::test]
async fn archive_child_logically_archives_failed_child_and_keeps_sandbox() {
    let fx = Fx::new().await;
    let sandbox = fx.dir.path().join("child-worktree");
    std::fs::create_dir(&sandbox).unwrap();
    std::fs::write(sandbox.join("work.txt"), "uncommitted work\n").unwrap();
    fx.sql(
        "UPDATE sessions SET sandbox_root=?2 WHERE id=?1",
        vals![fx.child.to_string(), sandbox.to_string_lossy().into_owned()],
    )
    .await;
    let result = fx.archive(fx.lead, fx.child).await.unwrap();
    assert_eq!(result.archived_session_ids, vec![fx.child]);
    assert_eq!(result.prior_status, SessionStatus::Failed);
    assert!(!result.deduplicated);
    assert!(result.sandbox_retained);
    assert_eq!(result.lead_generation, 1);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Archived);
    let row = fx
        .manager
        .store
        .lock()
        .await
        .get_session(fx.child)
        .unwrap()
        .unwrap();
    assert_eq!(row.sandbox_root.as_deref(), Some(sandbox.as_path()));
    assert_eq!(
        std::fs::read_to_string(sandbox.join("work.txt")).unwrap(),
        "uncommitted work\n"
    );
    assert!(!fx.manager.completed.read().await.contains_key(&fx.child));
}

#[tokio::test]
async fn archive_child_never_enters_cleanup_saga() {
    let fx = Fx::new().await;
    let sandbox = fx.dir.path().join("saga-shaped-worktree");
    std::fs::create_dir(&sandbox).unwrap();
    // Exactly the operator cleanup-saga route shape (leaf, Failed,
    // GitWorktree, cleanup state Live, !pending_archive).
    fx.sql(
        "UPDATE sessions SET sandbox_root=?2, sandbox_branch='rsi/child', sandbox_kind='GitWorktree',
                sandbox_cleanup_state='Live', pending_archive=0 WHERE id=?1",
        vals![fx.child.to_string(), sandbox.to_string_lossy().into_owned()],
    )
    .await;
    let row = fx
        .manager
        .store
        .lock()
        .await
        .get_session(fx.child)
        .unwrap()
        .unwrap();
    assert_eq!(row.sandbox_kind, Some(SandboxKind::GitWorktree));
    fx.archive(fx.lead, fx.child).await.unwrap();
    let row = fx
        .manager
        .store
        .lock()
        .await
        .get_session(fx.child)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, SessionStatus::Archived);
    assert_eq!(row.sandbox_cleanup_state, Some(SandboxCleanupState::Live));
    assert_eq!(row.sandbox_branch.as_deref(), Some("rsi/child"));
    assert_eq!(row.sandbox_root.as_deref(), Some(sandbox.as_path()));
    assert!(sandbox.is_dir());
}

#[tokio::test]
async fn archive_child_settles_pending_c5_marker_atomically() {
    let fx = Fx::new().await;
    fx.stage_c5(fx.child).await;
    let result = fx.archive(fx.lead, fx.child).await.unwrap();
    assert!(result.c5_marker_settled);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Archived);
    assert!(!fx.c5_marker(fx.child).await);
}

#[tokio::test]
async fn archive_child_c5_marker_survives_refused_archive() {
    let fx = Fx::new().await;
    fx.stage_c5(fx.child).await;
    fx.review_assignment(fx.child, "reserved", "sealed-work")
        .await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::ReviewSourceSealed,
        Some(Detail::AssignmentOpen),
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
    assert!(fx.c5_marker(fx.child).await);
}

#[tokio::test]
async fn archive_child_refuses_capacity_owned_failure() {
    let fx = Fx::new().await;
    let at = now();
    fx.sql_synthetic_refs(
        "INSERT INTO master_no_idle_capacity_incidents(
            incident_id,program_guard_job_id,controller_session_id,capacity_class,
            outage_epoch,state,backoff_bucket,wake_job_id,issue_id,project_id,
            provider,model,working_dir,last_capacity_model_invocation_id,
            last_terminal_sequence,next_due_slot,opened_at,updated_at,closed_at,close_reason
         ) VALUES(?1,?2,?3,'codex_usage_limit',1,'open',1,?4,NULL,NULL,
                  'Codex',NULL,'/var/tmp/archive-child',?5,0,?6,?6,?6,NULL,NULL)",
        vals![
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
            fx.child.to_string(),
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
            at
        ],
    )
    .await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::RecoveryOwnerHeld,
        Some(Detail::RecoveryOwner),
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

#[tokio::test]
async fn archive_child_refuses_operator_pause() {
    let fx = Fx::new().await;
    fx.sql(
        "INSERT INTO daemon_settings(key,value,updated_at) VALUES(?1,'true',?2)",
        vals![format!("manager_operator_pause:{}", fx.child), now()],
    )
    .await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::RecoveryOwnerHeld,
        Some(Detail::RecoveryOwner),
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

// --- Manager-mode parity (recovery_owner_gate extraction) -----------------

#[tokio::test]
async fn manager_action_human_gate_still_holds_on_c5_marker() {
    let fx = Fx::new().await;
    fx.stage_c5(fx.child).await;
    let store = fx.manager.store.lock().await;
    let error = store.manager_action_human_gate(fx.child).unwrap_err();
    assert!(
        matches!(&error, DaemonError::InvalidParam(code) if code == "manager_v2_human_or_recovery_owner"),
        "{error:?}"
    );
    let error = store
        .manager_action_human_gate_with_interrupted_resume(fx.child, true)
        .unwrap_err();
    drop(store);
    assert!(
        matches!(&error, DaemonError::InvalidParam(code) if code == "manager_v2_human_or_recovery_owner"),
        "{error:?}"
    );
}

#[tokio::test]
async fn manager_action_human_gate_still_refuses_program_human_gate() {
    let fx = Fx::new().await;
    fx.job("program_guard", fx.child, None).await;
    fx.assistant_output(fx.child, HUMAN_GATE).await;
    let error = fx
        .manager
        .store
        .lock()
        .await
        .manager_action_human_gate(fx.child)
        .unwrap_err();
    assert!(
        matches!(&error, DaemonError::InvalidParam(code) if code == "manager_v2_human_or_recovery_owner"),
        "{error:?}"
    );
}

// --- Program gate in agent-archive mode -----------------------------------

#[tokio::test]
async fn archive_child_refuses_program_human_gate_outcome() {
    let fx = Fx::new().await;
    fx.job("program_guard", fx.child, None).await;
    fx.assistant_output(fx.child, HUMAN_GATE).await;
    fx.stage_c5(fx.child).await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::RecoveryOwnerHeld,
        Some(Detail::RecoveryOwner),
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
    assert!(fx.c5_marker(fx.child).await);
}

#[tokio::test]
async fn archive_child_refuses_unknown_program_evidence() {
    let fx = Fx::new().await;
    fx.job("program_guard", fx.child, None).await;
    fx.assistant_output(fx.child, "orchestration_outcome_v1: {")
        .await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::RecoveryOwnerHeld,
        Some(Detail::ProgramEvidenceUnknown),
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

#[tokio::test]
async fn archive_child_allows_program_terminal_allowed_outcome() {
    let fx = Fx::new().await;
    fx.job("program_guard", fx.child, None).await;
    fx.assistant_output(fx.child, QUEUE_EXHAUSTED).await;
    fx.stage_c5(fx.child).await;
    let result = fx.archive(fx.lead, fx.child).await.unwrap();
    assert!(result.c5_marker_settled);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Archived);
    assert!(!fx.c5_marker(fx.child).await);
}

// --- Authority -------------------------------------------------------------

#[tokio::test]
async fn archive_child_allows_current_epic_lead() {
    let fx = Fx::new().await;
    let result = fx.archive(fx.lead, fx.child).await.unwrap();
    assert_eq!(result.archived_session_ids, vec![fx.child]);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Archived);
}

/// Decision A (review a755d660): archive authority is the current Epic lead
/// only. A leaf that spawned the child holds no archive authority; the lead
/// does.
#[tokio::test]
async fn archive_child_refuses_non_lead_spawn_owner_current_lead_only() {
    let fx = Fx::new().await;
    let owner = Uuid::new_v4();
    fx.insert(fx.session(owner, Some(fx.epic), SessionStatus::Running))
        .await;
    fx.spawn_request(owner, fx.child, 1).await;
    assert_refused(
        fx.archive(owner, fx.child).await,
        Code::TargetNotAuthorized,
        None,
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
    let result = fx.archive(fx.lead, fx.child).await.unwrap();
    assert_eq!(result.archived_session_ids, vec![fx.child]);
    assert_eq!(result.lead_generation, 1);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Archived);
}

/// Review round 2 (`former-lead-spawn-owner`): the lead that spawned a child
/// keeps its launched reservation after a handoff, but a lead's authority is
/// only the current-lead branch.
#[tokio::test]
async fn archive_child_refuses_former_lead_spawn_owner_after_handoff() {
    let fx = Fx::new().await;
    let second_lead = Uuid::new_v4();
    fx.insert(fx.session(second_lead, Some(fx.epic), SessionStatus::Running))
        .await;
    fx.spawn_request(fx.lead, fx.child, 1).await;
    fx.hand_lead_to(second_lead).await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::TargetNotAuthorized,
        None,
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
    // The new lead holds the authority the former lead lost.
    let result = fx.archive(second_lead, fx.child).await.unwrap();
    assert_eq!(result.archived_session_ids, vec![fx.child]);
    assert_eq!(result.lead_generation, 2);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Archived);
}

#[tokio::test]
async fn archive_child_refuses_former_lead_spawn_owner_dedup_replay() {
    let fx = Fx::new().await;
    let second_lead = Uuid::new_v4();
    fx.insert(fx.session(second_lead, Some(fx.epic), SessionStatus::Running))
        .await;
    fx.spawn_request(fx.lead, fx.child, 1).await;
    let mut events = fx.archived_events();
    let first = fx.archive(fx.lead, fx.child).await.unwrap();
    assert_eq!(first.archived_session_ids, vec![fx.child]);
    fx.hand_lead_to(second_lead).await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::TargetNotAuthorized,
        None,
    );
    let replay = fx.archive(second_lead, fx.child).await.unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.lead_generation, 2);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Archived);
    assert_eq!(count_archived(&mut events, fx.child), 1);
}

#[tokio::test]
async fn archive_child_refuses_direct_parent_that_is_not_owner() {
    let fx = Fx::new().await;
    let parent = Uuid::new_v4();
    let nested = Uuid::new_v4();
    fx.insert(fx.session(parent, Some(fx.epic), SessionStatus::Running))
        .await;
    fx.insert(fx.session(nested, Some(parent), SessionStatus::Failed))
        .await;
    assert_refused(
        fx.archive(parent, nested).await,
        Code::TargetNotAuthorized,
        None,
    );
    assert_eq!(fx.status(nested).await, SessionStatus::Failed);
}

#[tokio::test]
async fn archive_child_refuses_manager_session_control() {
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    use rsi_common::harness_manager_v2::{
        ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerOperatingModeV2,
        ManagerPolicyV2,
    };
    let fx = Fx::new().await;
    let manager_session = Uuid::new_v4();
    let mut row = fx.session(manager_session, None, SessionStatus::Running);
    row.session_kind = SessionKind::Standard;
    fx.insert(row).await;
    // A manager scope names a legal Epic, i.e. one under a Group.
    let group = Uuid::new_v4();
    let mut group_row = fx.session(group, None, SessionStatus::Running);
    group_row.session_kind = SessionKind::Group;
    fx.insert(group_row).await;
    fx.sql(
        "UPDATE sessions SET parent_id=?2 WHERE id=?1",
        vals![fx.epic.to_string(), group.to_string()],
    )
    .await;
    {
        let store = fx.manager.store.lock().await;
        let _ = store.insert_project(&rsi_common::types::Project {
            id: crate::store::d04_test_project_id(),
            name: "Archive project".into(),
            path: Some(fx.dir.path().to_path_buf()),
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        });
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: crate::store::d04_test_project_id(),
                session_id: manager_session,
                epic_ids: Some(vec![fx.epic]),
                expected_row_version: 0,
            })
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: crate::store::d04_test_project_id(),
                expected_scope_version: 1,
                expected_policy_version: 0,
                idempotency_key: "grant".into(),
                policy: ManagerPolicyV2 {
                    mode: ManagerOperatingModeV2::Execute,
                    capabilities: vec![ManagerCapabilityV2::SessionControl],
                    ..Default::default()
                },
            })
            .unwrap();
    }
    // Precondition: the generic mutation authority (as used by
    // AgentContinueChild) does admit this manager.
    let cursor = fx.request(fx.child).await;
    fx.manager
        .agent_control()
        .authorize_continue_child(
            manager_session,
            &rsi_common::agent_coordination::AgentContinueChildRequestV1 {
                target_session_id: fx.child,
                query: "continue".into(),
                expected_tip_session_id: cursor.expected_tip_session_id,
                expected_event_sequence: cursor.expected_event_sequence,
                expected_custody_generation: cursor.expected_custody_generation,
                idempotency_key: None,
            },
        )
        .await
        .expect("SessionControl manager holds generic mutation scope");
    assert_refused(
        fx.archive(manager_session, fx.child).await,
        Code::TargetNotAuthorized,
        None,
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

#[tokio::test]
async fn archive_child_refuses_former_lead_after_handoff() {
    let fx = Fx::new().await;
    let successor = Uuid::new_v4();
    fx.insert(fx.session(successor, Some(fx.epic), SessionStatus::Running))
        .await;
    let (epic, new_lead) = (fx.epic, successor);
    archive_child_test_seam::install(fx.child, move |store| {
        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
                params![epic.to_string(), new_lead.to_string()],
            )
            .unwrap();
    });
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::TargetNotAuthorized,
        None,
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

#[tokio::test]
async fn archive_child_refuses_tip_under_other_epic() {
    let fx = Fx::new().await;
    let other_epic = Uuid::new_v4();
    let mut epic_row = fx.session(other_epic, None, SessionStatus::Running);
    epic_row.session_kind = SessionKind::Epic;
    fx.insert(epic_row).await;
    fx.sql(
        "UPDATE sessions SET status='Completed' WHERE id=?1",
        vals![fx.child.to_string()],
    )
    .await;
    let mut tip = fx.session(Uuid::new_v4(), Some(other_epic), SessionStatus::Failed);
    tip.continued_from = Some(fx.child);
    let tip_id = tip.id;
    fx.insert(tip).await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::TargetNotAuthorized,
        None,
    );
    assert_eq!(fx.status(tip_id).await, SessionStatus::Failed);
}

// --- Target-shape and live-continuation refusals --------------------------

#[tokio::test]
async fn archive_child_refuses_self() {
    let fx = Fx::new().await;
    assert_refused(
        fx.archive(fx.lead, fx.lead).await,
        Code::SelfArchiveDenied,
        None,
    );
    assert_eq!(fx.status(fx.lead).await, SessionStatus::Running);
}

#[tokio::test]
async fn archive_child_refuses_running_target() {
    let fx = Fx::new().await;
    let running = Uuid::new_v4();
    fx.insert(fx.session(running, Some(fx.epic), SessionStatus::Running))
        .await;
    assert_refused(
        fx.archive(fx.lead, running).await,
        Code::TargetNotTerminal,
        None,
    );
    assert_eq!(fx.status(running).await, SessionStatus::Running);
}

/// A container can never be a child of the lead's Epic (illegal topology),
/// so it is refused at authority before any shape check runs.
#[tokio::test]
async fn archive_child_refuses_container() {
    let fx = Fx::new().await;
    assert_refused(
        fx.archive(fx.lead, fx.epic).await,
        Code::TargetNotAuthorized,
        None,
    );
    assert_eq!(fx.status(fx.epic).await, SessionStatus::Running);
}

#[tokio::test]
async fn archive_child_refuses_epic_lead_target() {
    let fx = Fx::new().await;
    // The child also leads a second Epic; its own Epic's lead may not archive
    // a session that another container depends on as its lead.
    let led_epic = Uuid::new_v4();
    let mut led_epic_row = fx.session(led_epic, None, SessionStatus::Running);
    led_epic_row.session_kind = SessionKind::Epic;
    led_epic_row.lead_session_id = Some(fx.child);
    fx.insert(led_epic_row).await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::TargetIsLead,
        None,
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

#[tokio::test]
async fn archive_child_refuses_running_successor() {
    let fx = Fx::new().await;
    let mut successor = fx.session(Uuid::new_v4(), Some(fx.epic), SessionStatus::Running);
    successor.continued_from = Some(fx.child);
    let successor_id = successor.id;
    fx.insert(successor).await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::LiveContinuation,
        Some(Detail::RunningSuccessor),
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
    assert_eq!(fx.status(successor_id).await, SessionStatus::Running);
}

/// The retained recovery-owner clause (an enabled non-sentinel resume wake)
/// runs before the live-continuation probes, so it is the refusing owner.
#[tokio::test]
async fn archive_child_refuses_enabled_resume_wake() {
    let fx = Fx::new().await;
    let job = fx.job("resume", fx.child, None).await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::RecoveryOwnerHeld,
        Some(Detail::RecoveryOwner),
    );
    assert!(fx.job_enabled(job.id).await);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

#[tokio::test]
async fn archive_child_refuses_live_retry_timer() {
    let fx = Fx::new().await;
    fx.manager
        .completed
        .write()
        .await
        .get_mut(&fx.child)
        .unwrap()
        .superseded_by_retry = Some(Uuid::new_v4());
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::LiveContinuation,
        Some(Detail::RetryTimer),
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

#[tokio::test]
async fn archive_child_refuses_fresh_relaunch_intent() {
    use crate::store::agent_child_relaunch_intents::{RelaunchIntentRow, RelaunchState};
    let fx = Fx::new().await;
    let request_id = Uuid::new_v4();
    fx.manager
        .store
        .lock()
        .await
        .insert_child_relaunch_intent(&RelaunchIntentRow {
            request_id,
            key_digest: digest('d'),
            request_fingerprint: digest('e'),
            caller_session_id: fx.lead,
            target_session_id: fx.child,
            tip_session_id: fx.child,
            observed_event_sequence: 0,
            observed_custody_generation: None,
            dedup_key: format!("agent.child_relaunch.v1:{request_id}"),
            state: RelaunchState::Intent,
            invocation_id: None,
            receipt_json: None,
            abandon_reason: None,
        })
        .unwrap();
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::LiveContinuation,
        Some(Detail::FreshRelaunchIntent),
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

/// Review a6423c3f `archive_pending_mail_coverage`: every pending mailbox
/// state targeting the child's lineage refuses the archive, and the mail stays
/// pending for its delivery owner.
#[tokio::test]
async fn archive_child_refuses_pending_mail_keeps_child_failed_and_mail_pending() {
    for state in ["queued", "claimed", "injected", "uncertain"] {
        let fx = Fx::new().await;
        let mail = fx.pending_mail(fx.child, state).await;
        let before = fx.row_snapshot("agent_messages", "id", mail).await;
        assert_refused(
            fx.archive(fx.lead, fx.child).await,
            Code::LiveContinuation,
            Some(Detail::PendingMail),
        );
        assert_eq!(fx.status(fx.child).await, SessionStatus::Failed, "{state}");
        assert_eq!(
            fx.row_snapshot("agent_messages", "id", mail).await,
            before,
            "{state}"
        );
        let pending: String = fx
            .manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT state FROM agent_messages WHERE id=?1",
                [mail.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, state);
    }
}

/// Review a6423c3f `archive_successor_reservation_coverage`: a live successor
/// reservation for the child refuses the archive and is left untouched.
#[tokio::test]
async fn archive_child_refuses_live_successor_reservation_keeps_reservation() {
    for state in ["reserved", "launching", "uncertain"] {
        let fx = Fx::new().await;
        let reservation = fx.successor_reservation(fx.child, state).await;
        let before = fx
            .row_snapshot(
                "agent_successor_reservations",
                "reservation_id",
                reservation,
            )
            .await;
        assert_refused(
            fx.archive(fx.lead, fx.child).await,
            Code::LiveContinuation,
            Some(Detail::SuccessorReservation),
        );
        assert_eq!(fx.status(fx.child).await, SessionStatus::Failed, "{state}");
        assert_eq!(
            fx.row_snapshot(
                "agent_successor_reservations",
                "reservation_id",
                reservation
            )
            .await,
            before,
            "{state}"
        );
    }
}

// --- Review source --------------------------------------------------------

#[tokio::test]
async fn archive_child_refuses_open_review_assignment() {
    let fx = Fx::new().await;
    fx.review_assignment(fx.child, "reserved", "open-work")
        .await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::ReviewSourceSealed,
        Some(Detail::AssignmentOpen),
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

#[tokio::test]
async fn archive_child_refuses_unverdicted_review_source() {
    let fx = Fx::new().await;
    fx.review_work_fact(fx.child, "unverdicted-work").await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::ReviewSourceSealed,
        Some(Detail::VerdictPending),
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

#[tokio::test]
async fn archive_child_allows_review_source_after_verdict() {
    let fx = Fx::new().await;
    fx.review_work_fact(fx.child, "verdicted-work").await;
    let assignment = fx
        .review_assignment(fx.child, "active", "verdicted-work")
        .await;
    fx.sql_synthetic_refs(
        "INSERT INTO manager_review_receipts (
             receipt_id, assignment_id, source_sha, reviewer_session_id, reviewer_invocation_id,
             reviewer_custody_id, reviewer_custody_generation, verdict, idempotency_key,
             request_fingerprint, created_at)
         SELECT ?1, assignment_id, source_sha, reviewer_session_id, reviewer_invocation_id,
                reviewer_custody_id, reviewer_custody_generation, 'accepted', 'verdict',
                request_fingerprint, ?2
           FROM manager_review_assignments WHERE assignment_id=?3",
        vals![Uuid::new_v4().to_string(), now(), assignment.to_string()],
    )
    .await;
    fx.sql(
        "UPDATE manager_review_assignments SET state='submitted', terminal_at=?2, updated_at=?2, row_version=2
          WHERE assignment_id=?1",
        vals![assignment.to_string(), now()],
    )
    .await;
    fx.archive(fx.lead, fx.child).await.unwrap();
    assert_eq!(fx.status(fx.child).await, SessionStatus::Archived);
}

// --- Cursor, dedup and replay authority -----------------------------------

#[tokio::test]
async fn archive_child_stale_cursor_returns_observed_witness() {
    let fx = Fx::new().await;
    let mut request = fx.request(fx.child).await;
    let observed_sequence = request.expected_event_sequence;
    request.expected_event_sequence += 7;
    let error = fx
        .manager
        .agent_archive_child(fx.lead, request)
        .await
        .unwrap_err();
    let envelope = refusal(&error);
    assert_eq!(envelope.code, Code::StaleArchive);
    let observed = envelope
        .observed
        .expect("stale refusal carries the witness");
    assert_eq!(observed.tip_session_id, fx.child);
    assert_eq!(observed.event_sequence, observed_sequence);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Failed);
}

#[tokio::test]
async fn archive_child_replay_is_deduplicated() {
    let fx = Fx::new().await;
    let mut events = fx.archived_events();
    let first = fx.archive(fx.lead, fx.child).await.unwrap();
    let replay = fx.archive(fx.lead, fx.child).await.unwrap();
    assert!(!first.deduplicated);
    assert!(replay.deduplicated);
    assert_eq!(replay.archived_session_ids, Vec::<Uuid>::new());
    assert_eq!(replay.lead_generation, first.lead_generation);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Archived);
    assert_eq!(count_archived(&mut events, fx.child), 1);
}

#[tokio::test]
async fn archive_child_replay_after_lead_handoff() {
    let fx = Fx::new().await;
    let second_lead = Uuid::new_v4();
    fx.insert(fx.session(second_lead, Some(fx.epic), SessionStatus::Running))
        .await;
    let mut events = fx.archived_events();
    let first = fx.archive(fx.lead, fx.child).await.unwrap();
    assert_eq!(first.lead_generation, 1);
    fx.hand_lead_to(second_lead).await;
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::TargetNotAuthorized,
        None,
    );
    let replay = fx.archive(second_lead, fx.child).await.unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.lead_generation, 2);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Archived);
    assert_eq!(count_archived(&mut events, fx.child), 1);
}

#[tokio::test]
async fn archive_child_dedup_replay_rechecks_authority_inside_transaction() {
    let fx = Fx::new().await;
    let second_lead = Uuid::new_v4();
    fx.insert(fx.session(second_lead, Some(fx.epic), SessionStatus::Running))
        .await;
    fx.archive(fx.lead, fx.child).await.unwrap();
    // The pre-check still sees L1 as lead; the handoff commits before the
    // transaction, which must refuse the replay rather than deduplicate it.
    let (epic, new_lead) = (fx.epic, second_lead);
    archive_child_test_seam::install(fx.child, move |store| {
        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
                params![epic.to_string(), new_lead.to_string()],
            )
            .unwrap();
    });
    assert_refused(
        fx.archive(fx.lead, fx.child).await,
        Code::TargetNotAuthorized,
        None,
    );
    assert_eq!(fx.status(fx.child).await, SessionStatus::Archived);
}

// --- Watches and serialization --------------------------------------------

#[tokio::test]
async fn archive_child_consumes_caller_watch_without_second_delivery() {
    let fx = Fx::new().await;
    fx.spawn_request(fx.lead, fx.child, 1).await;
    let watch = fx.job("on_terminal", fx.lead, Some(fx.child)).await;
    assert!(fx.job_enabled(watch.id).await);
    let mut events = fx.archived_events();
    let result = fx.archive(fx.lead, fx.child).await.unwrap();
    assert_eq!(result.watches_consumed, 1);
    // The terminal watch service skips disabled jobs, so the one
    // SessionArchived publish delivers nothing further to the caller.
    assert!(!fx.job_enabled(watch.id).await);
    assert_eq!(count_archived(&mut events, fx.child), 1);
    let witness: String = fx
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT state FROM agent_child_watch_witness
              WHERE owner_session_id=?1 AND child_session_id=?2",
            params![fx.lead.to_string(), fx.child.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(witness, "consumed");
}

#[tokio::test]
async fn archive_child_leaves_other_owner_watch_enabled() {
    let fx = Fx::new().await;
    let other = Uuid::new_v4();
    fx.insert(fx.session(other, Some(fx.epic), SessionStatus::Running))
        .await;
    let own = fx.job("on_terminal", fx.lead, Some(fx.child)).await;
    let foreign = fx.job("on_terminal", other, Some(fx.child)).await;
    let result = fx.archive(fx.lead, fx.child).await.unwrap();
    assert_eq!(result.watches_consumed, 1);
    assert!(!fx.job_enabled(own.id).await);
    assert!(fx.job_enabled(foreign.id).await);
}

#[tokio::test]
async fn archive_child_serializes_with_inflight_continue() {
    let fx = Arc::new(Fx::new().await);
    // An in-flight continuation holds the tip's spawn guard.
    let guard = super::spawn_single_flight::acquire_spawn_guard(fx.child).await;
    let request = fx.request(fx.child).await;
    let archive = {
        let fx = Arc::clone(&fx);
        tokio::spawn(async move { fx.manager.agent_archive_child(fx.lead, request).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !archive.is_finished(),
        "archive must wait for the spawn guard"
    );
    // The continuation relaunches the child before releasing the guard.
    fx.manager
        .store
        .lock()
        .await
        .update_session_status(fx.child, SessionStatus::Running)
        .unwrap();
    drop(guard);
    assert_refused(archive.await.unwrap(), Code::TargetNotTerminal, None);
    assert_eq!(fx.status(fx.child).await, SessionStatus::Running);
}
