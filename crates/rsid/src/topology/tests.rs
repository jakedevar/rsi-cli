//! Provider-free acceptance tests for the durable topology executor (#634,
//! plan §7 T2-A1..A14). A fake effect boundary allocates real Git worktree
//! sandboxes and records real `model_invocations` admissions, so custody,
//! pins, preservation and the dedup fence run for real; only the provider
//! process is simulated. A "restart" drops every handle and reopens the store
//! (a new daemon boot id) while the fake's external world survives.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use rsi_common::rpc::{ResolveTopologyAttemptParams, TopologyAttemptAction};
use rsi_common::types::{
    GraphExecutionUpdate, SandboxKind, SessionStatus, UntilCondition, WorkflowExecutionLookup,
    WorkflowExecutionStatus,
};
use rsi_graph::data::{NodeData, Value as GraphValue};
use rsi_graph::format::{EdgeDef, NodeDef, RepeatPolicy, WorkflowDefinition};
use tempfile::TempDir;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::sandbox::SandboxAllocator;
use crate::store::Store;
use crate::topology::catalog::{CommandOutcome, CommandPoll};
use crate::topology::custody::TopologyForkSource;
use crate::topology::executor::{
    AnswerContinuation, Executor, LaunchRequest, NodeEffects, SessionObservation, Step,
};
use crate::topology::land::{LandEnqueue, LandRequest, LandStatus};
use crate::topology::recovery::recover_after_restart;
use crate::topology::review::{
    ExtraRound, ExtraRoundRequest, OncallAcceptance, ReviewRequest, ReviewStatus,
};
use crate::topology::store::{self as rows, AttemptRow, AttemptStatus, NewAttempt, NewExecution};

// ─── fixture ────────────────────────────────────────────────────────────────

pub(super) fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Topology Test",
            "-c",
            "user.email=topology@test.invalid",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct FakeSession {
    status: SessionStatus,
    sandbox: PathBuf,
    content: String,
    waiting_since: Option<chrono::DateTime<chrono::Utc>>,
    /// #1641 S4a: the provider can resume this session (default: it cannot,
    /// so a restart cut keeps the plan §3.4 preserve/retry).
    resumable: bool,
    /// The restart journal still owns the session.
    restart_intent_pending: bool,
    stop_reason: Option<String>,
    waited_ms: Option<u64>,
    /// #1728: the conversation turn cursor (`MAX(sequence)`); a continuation
    /// of the session advances it.
    turn: i64,
}

/// The world outside the daemon: provider sessions and their sandboxes.
#[derive(Default)]
struct World {
    sessions: HashMap<Uuid, FakeSession>,
    /// `(dedup_key, session_id, base_commit)` per real launch.
    launches: Vec<(String, Uuid, String)>,
    interrupts: Vec<Uuid>,
    reclaims: Vec<Uuid>,
    released: Vec<Uuid>,
    /// Attempt ids of every real catalog-op start, across incarnations.
    command_starts: Vec<Uuid>,
    /// Stale process groups killed by a later incarnation.
    killed_groups: Vec<i32>,
    /// The review service lives outside the daemon and survives a restart:
    /// the scripted verdict of each next assignment (#1641),
    review_script: VecDeque<ReviewScript>,
    /// `(assignment id, request)` of every review request, in order,
    review_requests: Vec<(Uuid, ReviewRequest)>,
    /// and the verdicts already handed out, so a repeated poll agrees.
    review_verdicts: HashMap<Uuid, ReviewStatus>,
    /// The merge queue lives outside the daemon and survives a restart:
    /// `(entry id, request)` of every enqueue, in order,
    land_requests: Vec<(Uuid, LandRequest)>,
    /// the state each entry is in (absent: still pending),
    land_states: HashMap<Uuid, LandStatus>,
    /// and a scripted answer that replaces the next enqueues (a refusal).
    land_override: Option<LandEnqueue>,
    /// `(session id, prompt)` of every continuation of a finished node
    /// session (#1641 S3c).
    continues: Vec<(Uuid, String)>,
    /// While set, the next continuation is refused.
    refuse_continue: bool,
    /// An operator continuation of the node session got there first (#1728).
    competing_continue: bool,
    /// What the review ledger answers to "may one more round run?" (#1715);
    /// `None`: the node's own reviewer.
    extra_round: Option<ExtraRound>,
    /// Every "may one more round run?" question asked.
    extra_round_asks: Vec<ExtraRoundRequest>,
    /// Every on-call acceptance the executor recorded on the review ledger (#1740).
    oncall_acceptances: Vec<OncallAcceptance>,
    /// #1728: a competing continuation of the session starts between the
    /// executor's observation and the resume's guarded check.
    racing_continue: bool,
}

/// One catalog-op run of the current incarnation.
struct FakeRun {
    sandbox: PathBuf,
    outcome: Option<CommandOutcome>,
}

/// Process group the fake reports for every op.
const FAKE_PGID: i32 = 4242;

pub(super) struct Fake {
    world: Arc<StdMutex<World>>,
    store: Arc<Mutex<Store>>,
    allocator: SandboxAllocator,
    enabled: AtomicBool,
    published: StdMutex<Vec<GraphExecutionUpdate>>,
    /// Fail the next `release_sandbox` (a crash inside discard phase 2).
    fail_release_once: AtomicBool,
    /// One-shot suspension inside the reservation window: `(entered, go)`.
    reserve_hold: StdMutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    /// Catalog-op runs of this incarnation only (lost on restart).
    runs: Arc<StdMutex<HashMap<Uuid, FakeRun>>>,
    build_cap: std::sync::atomic::AtomicU32,
    /// One-shot suspension between a resolution's key pre-check and its
    /// recording transaction: `(entered, go)`.
    pub(super) record_hold: StdMutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    /// #1417: while set, the host-load admission holds every launch.
    hold_launches: AtomicBool,
    /// #1641 S4b: while set, `launch` is refused with the sandbox capacity
    /// code (-32029) carrying this `data.code`, before any session exists.
    capacity_refusal: StdMutex<Option<&'static str>>,
    /// #1641 S4b: while set, a command node's build slot is held with this kind.
    build_slot_hold: StdMutex<Option<&'static str>>,
    /// Every `launch_held` question the executor asked: `(unattended,
    /// attempt id)`.
    launch_asks: StdMutex<Vec<(bool, Uuid)>>,
    /// Refuse this many review requests before accepting one.
    review_request_failures: std::sync::atomic::AtomicU32,
    /// While set, every review request is refused with this message.
    review_refusal: StdMutex<Option<String>>,
    /// One-shot suspension inside an answer delivery, after the executor
    /// decided to send it and before the continuation's fence: `(entered, go)`.
    pub(super) answer_hold: StdMutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    /// While set, an answer delivery fails after its provider effect started
    /// (the claim is recorded, the answer was sent).
    pub(super) fail_answer_after_effect: AtomicBool,
    /// Every operator attention message the executor sent: `(level, text)`.
    notices: StdMutex<Vec<(String, String)>>,
}

/// What the fake review service decides for the next polled assignment.
#[derive(Clone, Debug)]
pub(super) enum ReviewScript {
    Accept,
    Changes(Vec<&'static str>),
    Unsettled(&'static str),
}

impl NodeEffects for Fake {
    async fn review_extra_round(&self, request: ExtraRoundRequest) -> ExtraRound {
        let mut world = self.world.lock().unwrap();
        world.extra_round_asks.push(request);
        world.extra_round.clone().unwrap_or(ExtraRound::Same)
    }

    async fn record_oncall_acceptance(&self, acceptance: OncallAcceptance) -> Result<()> {
        self.world
            .lock()
            .unwrap()
            .oncall_acceptances
            .push(acceptance);
        Ok(())
    }

    async fn request_review(&self, request: ReviewRequest) -> Result<Uuid> {
        if self
            .review_request_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(DaemonError::PolicyDenied("review service refused".into()));
        }
        if let Some(message) = self.review_refusal.lock().unwrap().clone() {
            return Err(DaemonError::PolicyDenied(message));
        }
        // Idempotent per attempt, like the production effect must be.
        let assignment = Uuid::new_v5(&Uuid::NAMESPACE_OID, request.attempt_id.as_bytes());
        let mut world = self.world.lock().unwrap();
        if !world
            .review_requests
            .iter()
            .any(|(id, _)| *id == assignment)
        {
            world.review_requests.push((assignment, request));
        }
        Ok(assignment)
    }

    async fn review_status(&self, assignment_id: Uuid) -> ReviewStatus {
        let mut world = self.world.lock().unwrap();
        if let Some(settled) = world.review_verdicts.get(&assignment_id) {
            return settled.clone();
        }
        let Some(script) = world.review_script.pop_front() else {
            return ReviewStatus::Pending;
        };
        let source_commit = world
            .review_requests
            .iter()
            .find(|(id, _)| *id == assignment_id)
            .map(|(_, request)| request.source_commit.clone())
            .expect("a polled assignment was requested");
        let status = match script {
            ReviewScript::Accept => ReviewStatus::Accepted {
                findings: Vec::new(),
                reviewed_commit: source_commit,
            },
            ReviewScript::Changes(findings) => ReviewStatus::ChangesRequested {
                findings: findings.into_iter().map(str::to_owned).collect(),
            },
            ReviewScript::Unsettled(reason) => ReviewStatus::Unsettled(reason.to_owned()),
        };
        world.review_verdicts.insert(assignment_id, status.clone());
        status
    }

    async fn find_land_entry(&self, request: &LandRequest) -> Option<Uuid> {
        let world = self.world.lock().unwrap();
        world
            .land_requests
            .iter()
            .find(|(_, other)| other.dedup_key == request.dedup_key)
            .map(|(entry, _)| *entry)
    }

    async fn enqueue_land(&self, request: LandRequest) -> Result<LandEnqueue> {
        let mut world = self.world.lock().unwrap();
        if let Some(answer) = world.land_override.clone() {
            return Ok(answer);
        }
        // Idempotent per replay key, like the queue's UNIQUE identity.
        if let Some((entry, _)) = world
            .land_requests
            .iter()
            .find(|(_, other)| other.dedup_key == request.dedup_key)
        {
            return Ok(LandEnqueue::Queued(*entry));
        }
        let entry = Uuid::new_v5(&Uuid::NAMESPACE_OID, request.dedup_key.as_bytes());
        world.land_requests.push((entry, request));
        Ok(LandEnqueue::Queued(entry))
    }

    async fn land_status(&self, entry_id: Uuid) -> LandStatus {
        self.world
            .lock()
            .unwrap()
            .land_states
            .get(&entry_id)
            .cloned()
            .unwrap_or(LandStatus::Pending)
    }

    async fn before_resolution_record(&self, _execution_id: Uuid) {
        let hold = self.record_hold.lock().unwrap().take();
        if let Some((entered, go)) = hold {
            entered.notify_one();
            go.notified().await;
        }
    }

    async fn before_reserve(&self, _execution_id: Uuid) {
        let hold = self.reserve_hold.lock().unwrap().take();
        if let Some((entered, go)) = hold {
            entered.notify_one();
            go.notified().await;
        }
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    async fn launch(&self, request: LaunchRequest) -> Result<Uuid> {
        if let Some(code) = *self.capacity_refusal.lock().unwrap() {
            return Err(DaemonError::StructuredRpc {
                rpc_code: -32029,
                message: "sandbox_capacity_refused".into(),
                data: serde_json::json!({"kind": "sandbox_capacity", "code": code}),
            });
        }
        let key = request
            .config
            .model_invocation_dedup_key
            .clone()
            .expect("topology launches carry a dedup key");
        {
            // The same UNIQUE fence production admission hits.
            let store = self.store.lock().await;
            admit_invocation(&store, request.session_id, &key).map_err(|error| {
                DaemonError::PolicyDenied(format!("duplicate model invocation: {error}"))
            })?;
        }
        let mut world = self.world.lock().unwrap();
        let allocation = self.allocator.allocate(
            request.session_id,
            request.fork.origin(),
            SandboxKind::GitWorktree,
            request.fork.commit(),
            None,
        )?;
        world
            .launches
            .push((key, request.session_id, request.fork.commit().to_owned()));
        world.sessions.insert(
            request.session_id,
            FakeSession {
                status: SessionStatus::Running,
                sandbox: allocation.root,
                content: String::new(),
                waiting_since: None,
                resumable: false,
                restart_intent_pending: false,
                stop_reason: None,
                waited_ms: None,
                turn: 0,
            },
        );
        Ok(request.session_id)
    }

    async fn session(&self, session_id: Uuid) -> Option<SessionObservation> {
        let world = self.world.lock().unwrap();
        world
            .sessions
            .get(&session_id)
            .map(|session| SessionObservation {
                status: session.status,
                sandbox_root: Some(session.sandbox.clone()),
                waiting_since: session.waiting_since,
                resumable: session.resumable,
                restart_intent_pending: session.restart_intent_pending,
                stop_reason: session.stop_reason.clone(),
                waited_ms: session.waited_ms,
                restart_cut_fence: Some(crate::store::manager_actions::fence::RestartCutFenceV1 {
                    status: session.status,
                    stop_reason: session.stop_reason.clone(),
                    restart_intent_pending: session.restart_intent_pending,
                    event_sequence: session.turn,
                    invocation_id: None,
                    custody_generation: None,
                }),
            })
    }

    async fn output(&self, session_id: Uuid) -> Result<NodeData> {
        let world = self.world.lock().unwrap();
        let mut data = NodeData::new();
        if let Some(session) = world.sessions.get(&session_id) {
            data.insert("content", GraphValue::String(session.content.clone()));
        }
        data.insert("_completed", GraphValue::Bool(true));
        Ok(data)
    }

    async fn interrupt(&self, session_id: Uuid) {
        let mut world = self.world.lock().unwrap();
        world.interrupts.push(session_id);
        if let Some(session) = world.sessions.get_mut(&session_id)
            && !session.status.is_terminal()
        {
            session.status = SessionStatus::Interrupted;
        }
    }

    async fn reclaim(&self, session_id: Uuid) {
        self.world.lock().unwrap().reclaims.push(session_id);
    }

    async fn release_sandbox(&self, session_id: Uuid) -> Result<()> {
        if self.fail_release_once.swap(false, Ordering::SeqCst) {
            return Err(DaemonError::Process(
                "simulated crash while archiving".into(),
            ));
        }
        let mut world = self.world.lock().unwrap();
        if !world.released.contains(&session_id) {
            world.released.push(session_id);
        }
        Ok(())
    }

    async fn predicate_met(&self, _predicate: &str, _project_id: Option<Uuid>) -> bool {
        false
    }

    fn publish(&self, update: GraphExecutionUpdate) {
        self.published.lock().unwrap().push(update);
    }

    async fn allocate_command_sandbox(
        &self,
        session_id: Uuid,
        fork: TopologyForkSource,
    ) -> Result<PathBuf> {
        crate::topology::catalog::allocate_or_adopt(&self.allocator, session_id, &fork)
    }

    async fn start_command(
        &self,
        attempt_id: Uuid,
        sandbox: PathBuf,
        _op: rsi_common::types::CatalogOp,
    ) -> Result<Option<i32>> {
        self.world.lock().unwrap().command_starts.push(attempt_id);
        self.runs.lock().unwrap().insert(
            attempt_id,
            FakeRun {
                sandbox,
                outcome: None,
            },
        );
        Ok(Some(FAKE_PGID))
    }

    fn poll_command(&self, attempt_id: Uuid) -> Option<CommandPoll> {
        let outcome = self.runs.lock().unwrap().get(&attempt_id)?.outcome.clone();
        Some(outcome.map_or(
            CommandPoll::Running {
                pgid: Some(FAKE_PGID),
            },
            CommandPoll::Exited,
        ))
    }

    fn cancel_command(&self, attempt_id: Uuid) {
        if let Some(run) = self.runs.lock().unwrap().get_mut(&attempt_id) {
            run.outcome.get_or_insert_with(|| CommandOutcome {
                exit_code: -1,
                ..CommandOutcome::default()
            });
        }
    }

    fn forget_command(&self, attempt_id: Uuid) {
        self.runs.lock().unwrap().remove(&attempt_id);
    }

    fn kill_stale_group(&self, pgid: i32, _sandbox: Option<&std::path::Path>) {
        self.world.lock().unwrap().killed_groups.push(pgid);
    }

    fn build_node_cap(&self) -> u32 {
        self.build_cap.load(Ordering::SeqCst)
    }

    fn launch_held(
        &self,
        unattended: bool,
        _since: chrono::DateTime<chrono::Utc>,
        attempt: &AttemptRow,
    ) -> bool {
        self.launch_asks
            .lock()
            .unwrap()
            .push((unattended, attempt.id));
        self.hold_launches.load(Ordering::SeqCst) && unattended
    }

    fn build_slot_held(&self, _attempt: &AttemptRow) -> Option<&'static str> {
        *self.build_slot_hold.lock().unwrap()
    }

    fn notify_operator(&self, level: &str, message: String) {
        self.notices.lock().unwrap().push((level.into(), message));
    }

    async fn continue_with_answer(&self, request: AnswerContinuation) -> Result<()> {
        let hold = self.answer_hold.lock().unwrap().take();
        if let Some((entered, go)) = hold {
            entered.notify_one();
            go.notified().await;
        }
        if self.world.lock().unwrap().refuse_continue {
            return Err(DaemonError::InvalidParam("provider refused".into()));
        }
        // The daemon's continuation: the exact binding is rechecked and the
        // delivery durably claimed before the provider effect.
        {
            let store = self.store.lock().await;
            rows::claim_answer_delivery(&store, &request.binding, request.session_id)?;
        }
        {
            let mut world = self.world.lock().unwrap();
            let session = world
                .sessions
                .get_mut(&request.session_id)
                .ok_or_else(|| DaemonError::InvalidParam("no such session".into()))?;
            session.status = SessionStatus::Running;
            world.continues.push((request.session_id, request.prompt));
        }
        if self.fail_answer_after_effect.load(Ordering::SeqCst) {
            return Err(DaemonError::Process("daemon lost the provider".into()));
        }
        Ok(())
    }

    async fn continue_session(&self, session_id: Uuid, prompt: String) -> Result<()> {
        let mut world = self.world.lock().unwrap();
        if world.refuse_continue {
            return Err(DaemonError::InvalidParam("provider refused".into()));
        }
        let session = world
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| DaemonError::InvalidParam("no such session".into()))?;
        session.status = SessionStatus::Running;
        world.continues.push((session_id, prompt));
        Ok(())
    }

    async fn resume_cut_session(
        &self,
        session_id: Uuid,
        prompt: String,
        observed_restart_cut: Option<crate::store::manager_actions::fence::RestartCutFenceV1>,
    ) -> Result<()> {
        {
            let mut world = self.world.lock().unwrap();
            if world.racing_continue
                && let Some(session) = world.sessions.get_mut(&session_id)
            {
                // The operator's continuation starts after the observation.
                session.turn += 1;
                session.status = SessionStatus::Running;
            }
            let turn = world.sessions.get(&session_id).map(|session| session.turn);
            let observed_event_sequence = observed_restart_cut.map(|fence| fence.event_sequence);
            if observed_event_sequence.is_some()
                && turn.is_some()
                && observed_event_sequence != turn
            {
                return Err(DaemonError::InvalidParam(
                    "continuation_turn_changed".into(),
                ));
            }
        }
        if self.world.lock().unwrap().competing_continue {
            // The competing continuation owns the session and runs it.
            let mut world = self.world.lock().unwrap();
            if let Some(session) = world.sessions.get_mut(&session_id) {
                session.status = SessionStatus::Running;
            }
            return Err(DaemonError::InvalidParam(format!(
                "continuation_target_busy:{session_id}"
            )));
        }
        self.continue_session(session_id, prompt).await
    }
}

fn admit_invocation(store: &Store, session_id: Uuid, key: &str) -> rusqlite::Result<usize> {
    store.conn.execute(
        "INSERT INTO model_invocations (id,purpose,invocation_kind,foreground,paid_risk,\
         admission_status,status,trigger_source,session_id,policy_snapshot_json,dedup_key,created_at) \
         VALUES (?1,'workflow.graph.node','model','background','paid_capable','admitted',\
         'running','topology_tests',?2,'{}',?3,?4)",
        rusqlite::params![
            Uuid::new_v4().to_string(),
            session_id.to_string(),
            key,
            rows::now_text()
        ],
    )
}

pub(super) struct Harness {
    _dirs: (TempDir, TempDir),
    pub(super) repo: PathBuf,
    pub(super) base: String,
    db: PathBuf,
    sandboxes: PathBuf,
    world: Arc<StdMutex<World>>,
    pub(super) executor: Executor<Fake>,
}

impl Harness {
    pub(super) fn new() -> Self {
        let repo_dir = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let repo = repo_dir.path().canonicalize().unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("file.txt"), "base\n").unwrap();
        git(&repo, &["add", "file.txt"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        let db = state.path().join("rsi.db");
        let sandboxes = state.path().join("sandboxes");
        let world = Arc::new(StdMutex::new(World::default()));
        let executor = boot(&db, &sandboxes, &world);
        Self {
            _dirs: (repo_dir, state),
            repo,
            base,
            db,
            sandboxes,
            world,
            executor,
        }
    }

    /// Drop every daemon-side handle and reopen: a new daemon incarnation.
    pub(super) fn restart(&mut self) {
        let fresh = boot(&self.db, &self.sandboxes, &self.world);
        let old = std::mem::replace(&mut self.executor, fresh);
        drop(old);
    }

    pub(super) async fn start(&self, workflow: WorkflowDefinition) -> Uuid {
        let custody_plan = crate::session::graph_runner::plan_workflow_custody(&workflow).unwrap();
        let new = NewExecution {
            id: Uuid::new_v4(),
            topology_id: None,
            workflow_id: Uuid::new_v4(),
            definition: workflow,
            custody_plan,
            project_id: None,
            parent_session_id: None,
            repo_root: self.repo.clone(),
            base_commit: self.base.clone(),
            input: None,
            requester: None,
            owner: None,
        };
        let store = self.executor.store.lock().await;
        rows::insert_execution(&store, &new).unwrap();
        new.id
    }

    pub(super) async fn attempts(&self, execution_id: Uuid) -> Vec<AttemptRow> {
        let store = self.executor.store.lock().await;
        rows::load_attempts(&store, execution_id).unwrap()
    }

    pub(super) async fn attempt(
        &self,
        execution_id: Uuid,
        node: &str,
        iteration: u32,
        attempt: u32,
    ) -> AttemptRow {
        self.attempts(execution_id)
            .await
            .into_iter()
            .find(|row| {
                row.node_id == node && row.iteration == iteration && row.attempt_no == attempt
            })
            .unwrap_or_else(|| panic!("attempt {node}@{iteration}#{attempt} missing"))
    }

    pub(super) async fn status(&self, execution_id: Uuid) -> rows::ExecutionStatus {
        let store = self.executor.store.lock().await;
        rows::load_execution(&store, execution_id)
            .unwrap()
            .unwrap()
            .status
    }

    pub(super) async fn row_version(&self, execution_id: Uuid) -> i64 {
        let store = self.executor.store.lock().await;
        rows::current_row_version(&store, execution_id).unwrap()
    }

    pub(super) fn launches(&self) -> Vec<(String, Uuid, String)> {
        self.world.lock().unwrap().launches.clone()
    }

    pub(super) fn sandbox(&self, session_id: Uuid) -> PathBuf {
        self.world.lock().unwrap().sessions[&session_id]
            .sandbox
            .clone()
    }

    /// The daemon's own record of when the session began waiting (#1704).
    pub(super) fn set_waiting_since(&self, session_id: Uuid, since: chrono::DateTime<chrono::Utc>) {
        self.world
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session_id)
            .unwrap()
            .waiting_since = Some(since);
    }

    /// #1641 S4a: the session's provider can resume it.
    pub(super) fn set_resumable(&self, session_id: Uuid) {
        self.world
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session_id)
            .unwrap()
            .resumable = true;
    }

    /// #1641 S4a: the restart journal owns the (cut-off) session.
    pub(super) fn set_restart_intent_pending(&self, session_id: Uuid) {
        self.world
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session_id)
            .unwrap()
            .restart_intent_pending = true;
    }

    pub(super) fn clear_restart_intent_pending(&self, session_id: Uuid) {
        self.world
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session_id)
            .unwrap()
            .restart_intent_pending = false;
    }

    pub(super) fn interrupts(&self) -> Vec<Uuid> {
        self.world.lock().unwrap().interrupts.clone()
    }

    pub(super) fn set_stop_reason(&self, session_id: Uuid, reason: &str) {
        self.world
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session_id)
            .unwrap()
            .stop_reason = Some(reason.to_owned());
    }

    /// The session's own cumulative total of ended waits (#1704).
    pub(super) fn set_waited_ms(&self, session_id: Uuid, waited_ms: u64) {
        self.world
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session_id)
            .unwrap()
            .waited_ms = Some(waited_ms);
    }

    pub(super) fn set_status(&self, session_id: Uuid, status: SessionStatus) {
        self.world
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session_id)
            .unwrap()
            .status = status;
    }

    /// The node's agent commits one change and its session completes.
    pub(super) fn commit_and_complete(&self, session_id: Uuid, label: &str) -> String {
        let sandbox = self.sandbox(session_id);
        std::fs::write(sandbox.join(format!("{label}.txt")), label).unwrap();
        git(&sandbox, &["add", "-A"]);
        git(&sandbox, &["commit", "-q", "-m", label]);
        let mut world = self.world.lock().unwrap();
        let session = world.sessions.get_mut(&session_id).unwrap();
        session.status = SessionStatus::Completed;
        session.content =
            format!("PIPELINE HANDOFF — RESEARCH:\ndoc_path: /tmp/{label}.md\nstatus: complete\n");
        git(&sandbox, &["rev-parse", "HEAD"])
    }

    /// The node's agent stops on a BLOCKED handoff. `question` is its
    /// `blocker_question`; `class` its typed blocker class.
    pub(super) fn complete_blocked(&self, session_id: Uuid, class: &str, question: Option<&str>) {
        let mut world = self.world.lock().unwrap();
        let session = world.sessions.get_mut(&session_id).unwrap();
        session.status = SessionStatus::Completed;
        let mut content = format!(
            "PIPELINE HANDOFF — RESEARCH:\ndoc_path: /tmp/blocked.md\nstatus: blocked\n\
             blocker: cannot choose alone\nblocker_class: {class}\n\
             blocker_evidence: two designs fit\n"
        );
        if let Some(question) = question {
            content.push_str(&format!("blocker_question: {question}\n"));
        }
        session.content = content;
    }

    /// `(session id, prompt)` of every continuation so far.
    pub(super) fn continues(&self) -> Vec<(Uuid, String)> {
        self.world.lock().unwrap().continues.clone()
    }

    pub(super) fn refuse_continues(&self, refuse: bool) {
        self.world.lock().unwrap().refuse_continue = refuse;
    }

    pub(super) fn race_continues(&self, race: bool) {
        self.world.lock().unwrap().racing_continue = race;
    }

    pub(super) fn compete_continues(&self, compete: bool) {
        self.world.lock().unwrap().competing_continue = compete;
    }

    /// The review service decides these verdicts for the next assignments.
    pub(super) fn script_reviews(&self, script: impl IntoIterator<Item = ReviewScript>) {
        self.world.lock().unwrap().review_script.extend(script);
    }

    fn land_requests(&self) -> Vec<(Uuid, LandRequest)> {
        self.world.lock().unwrap().land_requests.clone()
    }

    fn set_land_state(&self, entry: Uuid, status: LandStatus) {
        self.world.lock().unwrap().land_states.insert(entry, status);
    }

    /// Operator attention messages sent so far.
    pub(super) fn operator_notices(&self) -> Vec<(String, String)> {
        self.executor.effects.notices.lock().unwrap().clone()
    }

    pub(super) fn review_requests(&self) -> Vec<(Uuid, ReviewRequest)> {
        self.world.lock().unwrap().review_requests.clone()
    }

    /// The review ledger answers "may one more round run?" with `round` (#1715).
    pub(super) fn script_extra_round(&self, round: ExtraRound) {
        self.world.lock().unwrap().extra_round = Some(round);
    }

    /// Every "may one more round run?" question the executor asked.
    pub(super) fn oncall_acceptances(&self) -> Vec<OncallAcceptance> {
        self.world.lock().unwrap().oncall_acceptances.clone()
    }

    pub(super) fn extra_round_asks(&self) -> Vec<ExtraRoundRequest> {
        self.world.lock().unwrap().extra_round_asks.clone()
    }

    /// The running catalog op of `attempt_id` exits with `exit_code`.
    pub(super) fn finish_command(&self, attempt_id: Uuid, exit_code: i32) {
        self.executor
            .effects
            .runs
            .lock()
            .unwrap()
            .get_mut(&attempt_id)
            .expect("command run")
            .outcome = Some(CommandOutcome {
            exit_code,
            duration_ms: 7,
            stdout_tail: format!("test result: ok. exit {exit_code}\n"),
            stderr_tail: String::new(),
            report: None,
            timed_out: false,
        });
    }

    /// The sandbox of a running catalog op (current incarnation).
    fn command_sandbox(&self, attempt_id: Uuid) -> PathBuf {
        self.executor.effects.runs.lock().unwrap()[&attempt_id]
            .sandbox
            .clone()
    }

    /// Run the executor, completing every running node, until it settles.
    pub(super) async fn run_to_end(&self, execution_id: Uuid) -> Step {
        for _ in 0..64 {
            let step = self.executor.advance(execution_id).await.unwrap();
            if step != Step::Wait {
                return step;
            }
            for attempt in self.attempts(execution_id).await {
                if attempt.status == AttemptStatus::Running && attempt.node_kind == "command" {
                    self.finish_command(attempt.id, 0);
                } else if attempt.status == AttemptStatus::Running {
                    self.commit_and_complete(
                        attempt.session_id,
                        &format!(
                            "{}-{}-{}",
                            attempt.node_id, attempt.iteration, attempt.attempt_no
                        ),
                    );
                }
            }
        }
        panic!("execution did not settle");
    }

    async fn resolve(
        &self,
        execution_id: Uuid,
        attempt_id: Uuid,
        action: TopologyAttemptAction,
        key: &str,
        confirm: Option<&str>,
    ) -> Result<rsi_common::rpc::ResolveTopologyAttemptResponse> {
        let version = self.row_version(execution_id).await;
        self.resolve_at(execution_id, attempt_id, action, key, confirm, version)
            .await
    }

    async fn resolve_at(
        &self,
        execution_id: Uuid,
        attempt_id: Uuid,
        action: TopologyAttemptAction,
        key: &str,
        confirm: Option<&str>,
        expected_row_version: i64,
    ) -> Result<rsi_common::rpc::ResolveTopologyAttemptResponse> {
        let params = ResolveTopologyAttemptParams {
            execution_id,
            attempt_id,
            action,
            expected_row_version,
            idempotency_key: key.to_owned(),
            confirm_preserved_commit: confirm.map(str::to_owned),
        };
        self.executor.resolve_attempt(&params).await
    }
}

fn boot(db: &Path, sandboxes: &Path, world: &Arc<StdMutex<World>>) -> Executor<Fake> {
    let store = Store::open(db).unwrap();
    let boot_id = store.program_run_boot_id();
    let store = Arc::new(Mutex::new(store));
    let fake = Fake {
        world: Arc::clone(world),
        store: Arc::clone(&store),
        allocator: SandboxAllocator::new(sandboxes.to_path_buf()),
        enabled: AtomicBool::new(true),
        published: StdMutex::new(Vec::new()),
        fail_release_once: AtomicBool::new(false),
        reserve_hold: StdMutex::new(None),
        runs: Arc::default(),
        build_cap: std::sync::atomic::AtomicU32::new(2),
        record_hold: StdMutex::new(None),
        hold_launches: AtomicBool::new(false),
        capacity_refusal: StdMutex::new(None),
        build_slot_hold: StdMutex::new(None),
        launch_asks: StdMutex::new(Vec::new()),
        review_request_failures: std::sync::atomic::AtomicU32::new(0),
        review_refusal: StdMutex::new(None),
        answer_hold: StdMutex::new(None),
        fail_answer_after_effect: AtomicBool::new(false),
        notices: StdMutex::new(Vec::new()),
    };
    Executor::new(store, Arc::new(fake), boot_id, Arc::default())
}

fn workflow(nodes: &[&str], edges: &[(&str, &str)]) -> WorkflowDefinition {
    WorkflowDefinition {
        version: "1.0".into(),
        name: "durable".into(),
        description: String::new(),
        nodes: nodes
            .iter()
            .map(|id| {
                let mut node = NodeDef::action(*id, *id);
                node.instructions = format!("do {id}");
                node
            })
            .collect(),
        edges: edges
            .iter()
            .map(|(from, to)| EdgeDef::new(*from, *to))
            .collect(),
        metadata: std::collections::BTreeMap::default(),
    }
}

/// Attach typed steps and edge routing to a workflow snapshot, exactly as
/// the bridge stamps them.
fn with_steps(
    workflow: WorkflowDefinition,
    steps: &[(&str, serde_json::Value)],
    routing: &[(&str, &str, &str)],
) -> WorkflowDefinition {
    let workflow = stamp_steps(workflow, steps, routing);
    crate::topology::steps::validate_workflow(&workflow).expect("valid steps");
    workflow
}

/// `with_steps` without the validity assertion, for refusal tests.
fn stamp_steps(
    mut workflow: WorkflowDefinition,
    steps: &[(&str, serde_json::Value)],
    routing: &[(&str, &str, &str)],
) -> WorkflowDefinition {
    let steps: serde_json::Map<String, serde_json::Value> = steps
        .iter()
        .map(|(node, step)| ((*node).to_owned(), step.clone()))
        .collect();
    workflow.metadata.insert(
        crate::topology::steps::STEPS_METADATA_KEY.into(),
        GraphValue::String(serde_json::Value::Object(steps).to_string()),
    );
    let routing: Vec<serde_json::Value> = routing
        .iter()
        .map(|(from, to, when)| serde_json::json!({"from": from, "to": to, "when": when}))
        .collect();
    workflow.metadata.insert(
        crate::topology::steps::EDGE_WHEN_METADATA_KEY.into(),
        GraphValue::String(serde_json::Value::Array(routing).to_string()),
    );
    workflow
}

fn with_loops(
    mut workflow: WorkflowDefinition,
    loop_edges: &[(&str, &str)],
    regions: &[&[&str]],
    until: &UntilCondition,
) -> WorkflowDefinition {
    for (from, to) in loop_edges {
        workflow.edges.push(EdgeDef::new(*from, *to));
    }
    let loops: Vec<serde_json::Value> = loop_edges
        .iter()
        .map(|(from, to)| serde_json::json!({ "from": from, "to": to }))
        .collect();
    workflow.metadata.insert(
        "loop_edges".into(),
        GraphValue::String(serde_json::to_string(&loops).unwrap()),
    );
    workflow.metadata.insert(
        "scc_regions".into(),
        GraphValue::String(serde_json::to_string(regions).unwrap()),
    );
    workflow.metadata.insert(
        "until_condition".into(),
        GraphValue::String(serde_json::to_string(until).unwrap()),
    );
    workflow
}

async fn reserve_only(harness: &Harness, execution_id: Uuid, node: &str) -> AttemptRow {
    {
        let store = harness.executor.store.lock().await;
        rows::transition_execution(
            &store,
            execution_id,
            &[rows::ExecutionStatus::Accepted],
            rows::ExecutionStatus::Running,
            None,
            None,
            None,
        )
        .unwrap();
        rows::reserve_attempts(
            &store,
            execution_id,
            &[NewAttempt {
                node_id: node.into(),
                iteration: 0,
                attempt_no: 1,
                base_commit: harness.base.clone(),
                query: format!("do {node}"),
                node_kind: "session",
                catalog_op: None,
                effect_class: None,
            }],
        )
        .unwrap();
    }
    harness.attempt(execution_id, node, 0, 1).await
}

pub(super) fn error_code(error: &DaemonError) -> String {
    match error {
        DaemonError::StructuredRpc { data, .. } => data["code"].as_str().unwrap().to_owned(),
        other => panic!("expected a typed resolution error, got {other}"),
    }
}

async fn block_on_interrupt(harness: &Harness, execution_id: Uuid) -> AttemptRow {
    let attempt = harness.attempt(execution_id, "A", 0, 1).await;
    harness.set_status(attempt.session_id, SessionStatus::Interrupted);
    assert_eq!(
        harness.executor.advance(execution_id).await.unwrap(),
        Step::Done
    );
    assert_eq!(
        harness.status(execution_id).await,
        rows::ExecutionStatus::Blocked
    );
    let blocked = harness.attempt(execution_id, "A", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    blocked
}

// ─── A1–A5: restart, dedup, loss, interrupt ─────────────────────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a_a1_blocked_handoff_blocks_execution_with_evidence() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let attempt = harness.attempt(execution, "A", 0, 1).await;
    {
        let mut world = harness.world.lock().unwrap();
        let session = world.sessions.get_mut(&attempt.session_id).unwrap();
        session.status = SessionStatus::Completed;
        session.content = "PIPELINE HANDOFF — RESEARCH:\ndoc_path: /tmp/report.md\nstatus: blocked\nblocker: access denied\nblocker_class: authority\nblocker_evidence: operator decision required\n".into();
    }
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    let blocked = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert_eq!(
        blocked.failure_class.as_deref(),
        Some(rows::failure::HANDOFF_BLOCKED)
    );
    assert!(
        blocked
            .error
            .unwrap()
            .contains("operator decision required")
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Blocked
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn blocked_handoff_question_without_a_manager_ledger_blocks_as_before() {
    // #1641 S3c: an operator-started execution has no project-manager ledger
    // to file a question on, so the old behaviour stands: the attempt blocks.
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    harness.executor.advance(execution).await.unwrap();
    let attempt = harness.attempt(execution, "A", 0, 1).await;
    harness.complete_blocked(attempt.session_id, "technical_impasse", Some("Which one?"));
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    let blocked = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert_eq!(
        blocked.failure_class.as_deref(),
        Some(rows::failure::HANDOFF_BLOCKED)
    );
    assert!(harness.continues().is_empty());
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Blocked
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a_a2_malformed_handoff_is_handoff_invalid() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let attempt = harness.attempt(execution, "A", 0, 1).await;
    {
        let mut world = harness.world.lock().unwrap();
        let session = world.sessions.get_mut(&attempt.session_id).unwrap();
        session.status = SessionStatus::Completed;
        session.content = "looks good".into();
    }
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    let failed = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(failed.status, AttemptStatus::Failed);
    assert_eq!(
        failed.failure_class.as_deref(),
        Some(rows::failure::HANDOFF_INVALID)
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Failed
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a_a7_legacy_session_workflow_still_runs() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
    assert_eq!(
        harness.attempt(execution, "A", 0, 1).await.status,
        AttemptStatus::Succeeded
    );
}

/// T3a-A8: typed handoff fields flow downstream; the full last message is
/// replaced by them unless the consumer sets `pass_content: true`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a_a8_typed_handoff_fields_flow_without_full_message_by_default() {
    let harness = Harness::new();
    let definition = with_steps(
        workflow(&["A", "B", "C"], &[("A", "B"), ("A", "C")]),
        &[(
            "C",
            serde_json::json!({"kind": "session", "pass_content": true}),
        )],
        &[],
    );
    let execution = harness.start(definition).await;
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let a = harness.attempt(execution, "A", 0, 1).await;
    let b = harness.attempt(execution, "B", 0, 1).await;
    let c = harness.attempt(execution, "C", 0, 1).await;
    let output = a.output.unwrap();
    assert_eq!(output["fields"]["handoff"]["status"], "complete");
    assert_eq!(output["fields"]["handoff"]["doc_path"], "/tmp/A-0-1.md");
    let result_commit = output["fields"]["result_commit"].as_str().unwrap();
    // B sees the typed fields (doc and commit), rendered in place of the
    // message: the replacement is present, so the message is not needed.
    assert!(b.query().contains("doc_path: /tmp/A-0-1.md"));
    assert!(
        b.query()
            .contains(&format!("result_commit: {result_commit}"))
    );
    assert!(!b.query().contains("PIPELINE HANDOFF"));
    // C opted in and receives the full last message as well.
    assert!(c.query().contains("doc_path: /tmp/A-0-1.md"));
    assert!(c.query().contains("content:\nPIPELINE HANDOFF — RESEARCH:"));
}

fn command_step(krate: &str) -> serde_json::Value {
    serde_json::json!({"kind": "command", "op": {"name": "cargo_test_focused", "crate": krate, "filter": "topology", "lib_only": true}})
}

/// Start `execution` and return its running command attempt `node`.
async fn running_command(harness: &Harness, execution: Uuid, node: &str) -> AttemptRow {
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let attempt = harness.attempt(execution, node, 0, 1).await;
    assert_eq!(attempt.status, AttemptStatus::Running);
    assert_eq!(attempt.node_kind, "command");
    assert_eq!(attempt.pre_head.as_deref(), Some(harness.base.as_str()));
    assert_eq!(attempt.process_group_id, Some(FAKE_PGID));
    attempt
}

/// T3a-A4 (R2-3): a catalog op that writes a tracked file is detected after
/// the fact: `sandbox_mutated`, preserved work blocks the execution, and a
/// restart never re-runs the op (one start in total).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a_a4_catalog_op_sandbox_write_detected_not_replayed() {
    let mut harness = Harness::new();
    let execution = harness
        .start(with_steps(
            workflow(&["T"], &[]),
            &[("T", command_step("rsid"))],
            &[],
        ))
        .await;
    let attempt = running_command(&harness, execution, "T").await;
    let sandbox = harness.command_sandbox(attempt.id);
    std::fs::write(sandbox.join("file.txt"), "written by a test\n").unwrap();
    harness.finish_command(attempt.id, 0);
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    let blocked = harness.attempt(execution, "T", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert_eq!(
        blocked.failure_class.as_deref(),
        Some(rows::failure::SANDBOX_MUTATED)
    );
    let preserved = blocked.preserved_commit.clone().unwrap();
    assert!(ref_exists(
        &harness.repo,
        blocked.preserved_ref.as_deref().unwrap()
    ));
    assert_eq!(
        git(&harness.repo, &["show", &format!("{preserved}:file.txt")]),
        "written by a test"
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Blocked
    );
    {
        let store = harness.executor.store.lock().await;
        let row = rows::load_execution(&store, execution).unwrap().unwrap();
        let reason = row.blocked_reason.unwrap();
        assert_eq!(reason["kind"], rows::failure::PRESERVED_WORK);
        assert_eq!(reason["failure_class"], rows::failure::SANDBOX_MUTATED);
    }

    harness.restart();
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    harness.executor.advance(execution).await.unwrap();
    assert_eq!(
        harness.world.lock().unwrap().command_starts,
        vec![attempt.id]
    );
    assert_eq!(harness.attempts(execution).await.len(), 1);
    assert_eq!(
        harness.attempt(execution, "T", 0, 1).await.status,
        AttemptStatus::Blocked
    );
}

/// T3a-A5: a restart interrupts a running op whose sandbox is unchanged
/// (build output under `target/` is ignored): the stale group is killed,
/// the attempt settles `interrupted`, and the op re-runs only as a new
/// attempt in a fresh sandbox — never resumed in place.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a_a5_catalog_op_clean_interrupt_reruns_as_new_attempt() {
    let mut harness = Harness::new();
    let definition = with_steps(
        workflow(&["T", "B"], &[("T", "B")]),
        &[("T", command_step("rsid"))],
        &[],
    );
    let execution = harness.start(definition).await;
    let first = running_command(&harness, execution, "T").await;
    let first_sandbox = harness.command_sandbox(first.id);
    std::fs::create_dir_all(first_sandbox.join("target/debug")).unwrap();
    std::fs::write(first_sandbox.join("target/debug/build.log"), "cache").unwrap();

    harness.restart();
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    let interrupted = harness.attempt(execution, "T", 0, 1).await;
    assert_eq!(interrupted.status, AttemptStatus::Interrupted);
    assert_eq!(
        interrupted.failure_class.as_deref(),
        Some(rows::failure::INTERRUPTED)
    );
    assert_eq!(harness.world.lock().unwrap().killed_groups, vec![FAKE_PGID]);
    // The interrupted op's build cache is reclaimed; its worktree stays.
    assert!(first_sandbox.join("file.txt").exists());
    assert!(!first_sandbox.join("target").exists());

    let second = harness.attempt(execution, "T", 0, 2).await;
    assert_eq!(second.status, AttemptStatus::Running);
    assert_ne!(second.id, first.id);
    assert_ne!(second.sandbox_root, first.sandbox_root);
    assert_eq!(
        harness.world.lock().unwrap().command_starts,
        vec![first.id, second.id]
    );
    harness.finish_command(second.id, 0);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let done = harness.attempt(execution, "T", 0, 2).await;
    assert_eq!(done.status, AttemptStatus::Succeeded);
    assert_eq!(done.result_commit.as_deref(), Some(harness.base.as_str()));
    let output = done.output.unwrap();
    assert_eq!(output["fields"]["op"], "cargo_test_focused");
    assert_eq!(output["fields"]["exit_code"], 0.0);
    // Downstream sees the command's typed tails.
    let b = harness.attempt(execution, "B", 0, 1).await;
    assert!(b.query().contains("op: cargo_test_focused"));
    assert!(b.query().contains("test result: ok. exit 0"));
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
}

/// T3a-A6: a gate routes on upstream typed output; the untaken branch is
/// `skipped` and skipping propagates; a nonzero command routes `failure`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a_a6_gate_routes_and_untaken_branch_is_skipped() {
    let harness = Harness::new();
    let definition = with_steps(
        workflow(
            &["A", "G", "Yes", "No", "NoAfter"],
            &[("A", "G"), ("G", "Yes"), ("G", "No"), ("No", "NoAfter")],
        ),
        &[(
            "G",
            serde_json::json!({"kind": "gate", "condition": {"op": "and", "args": [
                {"op": "eq", "path": "nodes.A.handoff.status", "value": "complete"},
                {"op": "exists", "path": "nodes.A.result_commit"}
            ]}}),
        )],
        &[("G", "Yes", "gate_true"), ("G", "No", "gate_false")],
    );
    let execution = harness.start(definition).await;
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let gate = harness.attempt(execution, "G", 0, 1).await;
    assert_eq!(gate.status, AttemptStatus::Succeeded);
    assert_eq!(gate.output.as_ref().unwrap()["fields"]["value"], true);
    assert!(
        gate.output.as_ref().unwrap()["fields"]["condition_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(
        harness.attempt(execution, "Yes", 0, 1).await.status,
        AttemptStatus::Succeeded
    );
    for skipped in ["No", "NoAfter"] {
        let attempt = harness.attempt(execution, skipped, 0, 1).await;
        assert_eq!(attempt.status, AttemptStatus::Skipped, "{skipped}");
        assert_eq!(
            attempt.failure_class.as_deref(),
            Some(rows::failure::DEAD_PATH)
        );
    }
    assert!(
        harness
            .launches()
            .iter()
            .all(|(key, ..)| !key.contains(":No:") && !key.contains(":NoAfter:"))
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );

    // A failing check routes its failure edge; the success edge is skipped.
    let harness = Harness::new();
    let definition = with_steps(
        workflow(&["T", "Fix", "Ship"], &[("T", "Fix"), ("T", "Ship")]),
        &[("T", command_step("rsid"))],
        &[("T", "Fix", "failure")],
    );
    let execution = harness.start(definition).await;
    let attempt = running_command(&harness, execution, "T").await;
    harness.finish_command(attempt.id, 101);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        harness
            .attempt(execution, "T", 0, 1)
            .await
            .failure_class
            .as_deref(),
        Some(rows::failure::EXIT_NONZERO)
    );
    let fix = harness.attempt(execution, "Fix", 0, 1).await;
    assert_eq!(fix.status, AttemptStatus::Succeeded);
    assert!(fix.query().contains("failure_class: exit_nonzero"));
    assert_eq!(
        harness.attempt(execution, "Ship", 0, 1).await.status,
        AttemptStatus::Skipped
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a2_completed_edge_runs_consumer_on_success() {
    let harness = Harness::new();
    let definition = with_steps(
        workflow(&["T", "Report"], &[("T", "Report")]),
        &[("T", command_step("rsid"))],
        &[("T", "Report", "completed")],
    );
    let execution = harness.start(definition).await;
    let command = running_command(&harness, execution, "T").await;
    harness.finish_command(command.id, 0);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);

    let completed = harness.attempt(execution, "T", 0, 1).await;
    let report = harness.attempt(execution, "Report", 0, 1).await;
    assert_eq!(completed.status, AttemptStatus::Succeeded);
    assert_eq!(report.status, AttemptStatus::Succeeded);
    assert_eq!(report.base_commit, completed.result_commit.unwrap());
    assert!(report.query().contains("op: cargo_test_focused"));
    assert!(report.query().contains("exit_code: 0"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a2_completed_edge_runs_consumer_on_failure_with_typed_output() {
    let harness = Harness::new();
    let definition = with_steps(
        workflow(&["T", "Report"], &[("T", "Report")]),
        &[("T", command_step("rsid"))],
        &[("T", "Report", "completed")],
    );
    let execution = harness.start(definition).await;
    let command = running_command(&harness, execution, "T").await;
    harness.finish_command(command.id, 101);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);

    let failed = harness.attempt(execution, "T", 0, 1).await;
    let report = harness.attempt(execution, "Report", 0, 1).await;
    assert_eq!(failed.status, AttemptStatus::Failed);
    assert_eq!(
        failed.failure_class.as_deref(),
        Some(rows::failure::EXIT_NONZERO)
    );
    assert_eq!(report.status, AttemptStatus::Succeeded);
    assert_eq!(report.base_commit, failed.base_commit);
    let query = report.query();
    assert!(query.contains("op: cargo_test_focused"));
    assert!(query.contains("exit_code: 101"));
    assert!(query.contains("stdout_tail:"));
    assert!(query.contains("stderr_tail:"));
    assert!(query.contains("failure_class: exit_nonzero"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a2_gate_reads_exit_code_from_completed_source() {
    let harness = Harness::new();
    let definition = with_steps(
        workflow(&["T", "G"], &[("T", "G")]),
        &[
            ("T", command_step("rsid")),
            (
                "G",
                serde_json::json!({"kind": "gate", "condition": {
                    "op": "eq", "path": "nodes.T.exit_code", "value": 101
                }}),
            ),
        ],
        &[("T", "G", "completed")],
    );
    let execution = harness.start(definition).await;
    let command = running_command(&harness, execution, "T").await;
    harness.finish_command(command.id, 101);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);

    let gate = harness.attempt(execution, "G", 0, 1).await;
    assert_eq!(gate.status, AttemptStatus::Succeeded);
    assert_eq!(gate.output.as_ref().unwrap()["fields"]["value"], true);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a2_failure_and_success_edges_unchanged() {
    for (exit_code, expected_success, expected_failure) in [
        (0, AttemptStatus::Succeeded, AttemptStatus::Skipped),
        (101, AttemptStatus::Skipped, AttemptStatus::Succeeded),
    ] {
        let harness = Harness::new();
        let definition = with_steps(
            workflow(
                &["T", "Success", "Failure"],
                &[("T", "Success"), ("T", "Failure")],
            ),
            &[("T", command_step("rsid"))],
            &[("T", "Failure", "failure")],
        );
        let execution = harness.start(definition).await;
        let command = running_command(&harness, execution, "T").await;
        harness.finish_command(command.id, exit_code);
        assert_eq!(harness.run_to_end(execution).await, Step::Done);

        assert_eq!(
            harness.attempt(execution, "Success", 0, 1).await.status,
            expected_success
        );
        assert_eq!(
            harness.attempt(execution, "Failure", 0, 1).await.status,
            expected_failure
        );
    }
}

/// `topology_max_concurrent_build_nodes` bounds concurrently running
/// catalog ops daemon-wide; a waiting op starts when a slot frees.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a_build_node_cap_bounds_concurrent_ops() {
    let harness = Harness::new();
    harness
        .executor
        .effects
        .build_cap
        .store(1, Ordering::SeqCst);
    let definition = with_steps(
        workflow(&["T1", "T2"], &[]),
        &[("T1", command_step("rsid")), ("T2", command_step("rsi"))],
        &[],
    );
    let execution = harness.start(definition).await;
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let starts = harness.world.lock().unwrap().command_starts.clone();
    assert_eq!(starts.len(), 1);
    let waiting = harness
        .attempts(execution)
        .await
        .into_iter()
        .find(|attempt| attempt.status == AttemptStatus::Reserved)
        .expect("second op waits for a build slot");
    harness.finish_command(starts[0], 0);
    harness.executor.advance(execution).await.unwrap();
    let starts = harness.world.lock().unwrap().command_starts.clone();
    assert_eq!(starts, vec![starts[0], waiting.id]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
}

/// T2-A1: a restart adopts the running node session instead of relaunching,
/// including a crash between the launch and the `running` record.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_a1_restart_adopts_running_session() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let launched = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(launched.status, AttemptStatus::Running);
    assert_eq!(harness.launches().len(), 1);

    // Crash after the provider started but before `running` was recorded.
    {
        let store = harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE topology_node_attempts SET status='launching' WHERE id=?1",
                [launched.id.to_string()],
            )
            .unwrap();
    }
    harness.restart();
    let report = recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(report.advanced, vec![(execution, Step::Wait)]);
    assert_eq!(harness.launches().len(), 1, "the live session is adopted");
    let adopted = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(adopted.status, AttemptStatus::Running);
    assert_eq!(adopted.session_id, launched.session_id);

    let head = harness.commit_and_complete(adopted.session_id, "A");
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
    assert_eq!(
        harness
            .attempt(execution, "A", 0, 1)
            .await
            .result_commit
            .as_deref(),
        Some(head.as_str())
    );
}

/// T2-A1: startup restore marks a provider that died with the daemon
/// `Failed`; the executor treats that as the restart interrupting the
/// attempt (clean ⇒ a new attempt, diverged ⇒ preserved work), never as a
/// node failure.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_a1_restart_failed_session_is_interrupted_not_failed() {
    let mut harness = Harness::new();
    let clean = harness.start(workflow(&["A"], &[])).await;
    let diverged = harness.start(workflow(&["A"], &[])).await;
    for execution in [clean, diverged] {
        assert_eq!(
            harness.executor.advance(execution).await.unwrap(),
            Step::Wait
        );
    }
    let first_clean = harness.attempt(clean, "A", 0, 1).await;
    let first_diverged = harness.attempt(diverged, "A", 0, 1).await;
    let sandbox = harness.sandbox(first_diverged.session_id);
    std::fs::write(sandbox.join("partial.txt"), "unsaved work\n").unwrap();
    harness.restart();
    for session in [first_clean.session_id, first_diverged.session_id] {
        harness.set_status(session, SessionStatus::Failed);
    }
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();

    let settled = harness.attempt(clean, "A", 0, 1).await;
    assert_eq!(settled.status, AttemptStatus::Interrupted);
    assert_eq!(
        settled.failure_class.as_deref(),
        Some(rows::failure::INTERRUPTED)
    );
    assert_eq!(
        harness.attempt(clean, "A", 0, 2).await.status,
        AttemptStatus::Running
    );
    assert_eq!(harness.status(clean).await, rows::ExecutionStatus::Running);

    let blocked = harness.attempt(diverged, "A", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert!(blocked.preserved_commit.is_some());
    assert_eq!(
        harness.status(diverged).await,
        rows::ExecutionStatus::Blocked
    );
}

/// #1641 S4a: a node session the restart cut off (a crash leaves it `Failed`,
/// an unjournaled graceful stop `Interrupted` with the restart cause) is
/// continued as the same session once for this boot, with its uncommitted
/// work in place. It is neither preserved nor retried.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn restart_interrupted_node_is_continued_not_preserved() {
    let mut harness = Harness::new();
    let crashed = harness.start(workflow(&["A"], &[])).await;
    let drained = harness.start(workflow(&["A"], &[])).await;
    let operator = harness.start(workflow(&["A"], &[])).await;
    for execution in [crashed, drained, operator] {
        assert_eq!(
            harness.executor.advance(execution).await.unwrap(),
            Step::Wait
        );
    }
    let first_crashed = harness.attempt(crashed, "A", 0, 1).await;
    let first_drained = harness.attempt(drained, "A", 0, 1).await;
    let first_operator = harness.attempt(operator, "A", 0, 1).await;
    for attempt in [&first_crashed, &first_drained, &first_operator] {
        let sandbox = harness.sandbox(attempt.session_id);
        std::fs::write(sandbox.join("partial.txt"), "unsaved work\n").unwrap();
        harness.set_resumable(attempt.session_id);
    }
    harness.restart();
    harness.set_status(first_crashed.session_id, SessionStatus::Failed);
    harness.set_status(first_drained.session_id, SessionStatus::Interrupted);
    harness.set_stop_reason(first_drained.session_id, "interrupted:deploy_drain");
    // An operator interrupt is the operator's decision, not a restart cut.
    harness.set_status(first_operator.session_id, SessionStatus::Interrupted);
    harness.set_stop_reason(first_operator.session_id, "interrupted:operator");
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();

    for (execution, first) in [(crashed, &first_crashed), (drained, &first_drained)] {
        let kept = harness.attempt(execution, "A", 0, 1).await;
        assert_eq!(kept.status, AttemptStatus::Running);
        assert_eq!(kept.session_id, first.session_id);
        assert_eq!(kept.failure_class, None);
        assert!(kept.preserved_commit.is_none());
        assert_eq!(kept.boot_id, Some(harness.executor.boot_id));
        assert_eq!(
            harness.status(execution).await,
            rows::ExecutionStatus::Running
        );
        assert_eq!(
            event_count(&harness, execution, "node_resumed_after_restart").await,
            1
        );
        assert_eq!(
            event_count(&harness, execution, "node_resume_refused").await,
            0
        );
        assert_eq!(
            harness
                .launches()
                .iter()
                .filter(|launch| launch.1 == first.session_id)
                .count(),
            1
        );
    }
    let continues = harness.continues();
    assert_eq!(continues.len(), 2, "one continuation per cut node");
    assert!(
        continues
            .iter()
            .all(|(_, prompt)| prompt.contains("Continue the task from its durable state"))
    );
    // The operator's interrupt keeps the plan §3.4 outcome.
    assert_eq!(
        harness.attempt(operator, "A", 0, 1).await.status,
        AttemptStatus::Blocked
    );

    // A second tick in the same boot does not resume it again.
    harness.executor.advance(crashed).await.unwrap();
    assert_eq!(harness.continues().len(), 2);
    // The continued session finishes with the uncommitted work it kept.
    let head = harness.commit_and_complete(first_crashed.session_id, "A");
    assert_eq!(harness.executor.advance(crashed).await.unwrap(), Step::Done);
    assert_eq!(
        harness.status(crashed).await,
        rows::ExecutionStatus::Succeeded
    );
    assert_eq!(
        harness
            .attempt(crashed, "A", 0, 1)
            .await
            .result_commit
            .as_deref(),
        Some(head.as_str())
    );
}

/// A continued session that dies again in the same boot is an ordinary node
/// failure: at most one auto-resume per attempt per boot.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn resumed_node_that_fails_again_is_not_resumed_twice() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    harness.executor.advance(execution).await.unwrap();
    let first = harness.attempt(execution, "A", 0, 1).await;
    harness.set_resumable(first.session_id);
    harness.restart();
    harness.set_status(first.session_id, SessionStatus::Failed);
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(harness.continues().len(), 1);
    harness.set_status(first.session_id, SessionStatus::Failed);
    harness.executor.advance(execution).await.unwrap();
    assert_eq!(harness.continues().len(), 1);
    let failed = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(failed.status, AttemptStatus::Failed);
    assert_eq!(
        failed.failure_class.as_deref(),
        Some(rows::failure::SESSION_FAILED)
    );
}

/// #1641 S4a: a session the restart journal still owns (a deploy drain cut
/// its turn and the startup pass has not continued it yet) is live. The
/// executor neither settles it nor continues it itself.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn restart_intent_pending_node_is_not_settled() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    harness.executor.advance(execution).await.unwrap();
    let first = harness.attempt(execution, "A", 0, 1).await;
    let sandbox = harness.sandbox(first.session_id);
    std::fs::write(sandbox.join("partial.txt"), "unsaved work\n").unwrap();
    harness.set_resumable(first.session_id);
    harness.restart();
    harness.set_status(first.session_id, SessionStatus::Interrupted);
    harness.set_stop_reason(first.session_id, "interrupted:deploy_drain");
    harness.set_restart_intent_pending(first.session_id);
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    let held = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(held.status, AttemptStatus::Running);
    assert_eq!(held.failure_class, None);
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Running
    );
    assert!(
        harness.continues().is_empty(),
        "the journal owns the continuation"
    );
    assert!(harness.interrupts().is_empty());
    assert_eq!(
        event_count(&harness, execution, "node_resumed_after_restart").await,
        0
    );

    // The startup pass continues the session; the attempt follows it.
    harness.set_status(first.session_id, SessionStatus::Running);
    harness.clear_restart_intent_pending(first.session_id);
    harness.executor.advance(execution).await.unwrap();
    assert_eq!(
        harness.attempt(execution, "A", 0, 1).await.status,
        AttemptStatus::Running
    );
    harness.commit_and_complete(first.session_id, "A");
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
}

/// #1722: cancelling an execution whose node session the restart journal
/// still holds settles it instead of waiting on the journal forever.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn cancelling_settles_a_journal_held_node() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    harness.executor.advance(execution).await.unwrap();
    let first = harness.attempt(execution, "A", 0, 1).await;
    harness.set_resumable(first.session_id);
    harness.restart();
    harness.set_status(first.session_id, SessionStatus::Interrupted);
    harness.set_stop_reason(first.session_id, "interrupted:deploy_drain");
    harness.set_restart_intent_pending(first.session_id);
    harness.executor.request_interrupt(execution).await.unwrap();
    for _ in 0..4 {
        if harness.executor.advance(execution).await.unwrap() == Step::Done {
            break;
        }
    }
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Cancelled
    );
    assert_eq!(
        harness.attempt(execution, "A", 0, 1).await.status,
        AttemptStatus::Cancelled
    );
    assert!(harness.continues().is_empty());
}

/// #1641 S4a: only a refused continuation falls back to plan §3.4. The refusal
/// is recorded once and the boot does not ask again.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn resume_refused_falls_back_to_preserve() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    harness.executor.advance(execution).await.unwrap();
    let first = harness.attempt(execution, "A", 0, 1).await;
    let sandbox = harness.sandbox(first.session_id);
    std::fs::write(sandbox.join("partial.txt"), "unsaved work\n").unwrap();
    harness.set_resumable(first.session_id);
    harness.refuse_continues(true);
    harness.restart();
    harness.set_status(first.session_id, SessionStatus::Failed);
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    let blocked = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(blocked.status, AttemptStatus::Blocked);
    assert_eq!(
        blocked.failure_class.as_deref(),
        Some(rows::failure::PRESERVED_WORK)
    );
    assert!(blocked.preserved_commit.is_some());
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Blocked
    );
    assert!(harness.continues().is_empty());
    assert_eq!(
        event_count(&harness, execution, "node_resume_refused").await,
        1
    );
    assert_eq!(
        event_count(&harness, execution, "node_resumed_after_restart").await,
        0
    );
}

/// #1728: an operator continuation of the same session got there first, so
/// the executor's fenced resume is refused busy. The session runs under the
/// operator's continuation: the executor neither preserves nor retries it and
/// does not start a second one.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn competing_continuation_defers_the_restart_resume() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    harness.executor.advance(execution).await.unwrap();
    let first = harness.attempt(execution, "A", 0, 1).await;
    let sandbox = harness.sandbox(first.session_id);
    std::fs::write(sandbox.join("partial.txt"), "unsaved work\n").unwrap();
    harness.set_resumable(first.session_id);
    harness.compete_continues(true);
    harness.restart();
    harness.set_status(first.session_id, SessionStatus::Failed);
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    let held = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(held.status, AttemptStatus::Running);
    assert_eq!(held.failure_class, None);
    assert!(held.preserved_commit.is_none());
    assert!(harness.continues().is_empty(), "exactly one continuation");
    assert_eq!(
        event_count(&harness, execution, "node_resume_refused").await,
        0
    );
    assert_eq!(
        event_count(&harness, execution, "node_resumed_after_restart").await,
        0
    );
    // The operator's continuation finishes the node; the attempt follows it.
    harness.commit_and_complete(first.session_id, "A");
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
}

/// #1728: an operator continuation of the same session starts after the
/// executor classified it restart-cut and before the resume's guarded check.
/// The resume carries the observed turn cursor, so it is refused: no second
/// invocation, nothing preserved from the stale observation, no recorded
/// refusal; the next observation follows the live session.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn stale_restart_resume_is_refused_after_a_competing_turn() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    harness.executor.advance(execution).await.unwrap();
    let first = harness.attempt(execution, "A", 0, 1).await;
    let sandbox = harness.sandbox(first.session_id);
    std::fs::write(sandbox.join("partial.txt"), "unsaved work\n").unwrap();
    harness.set_resumable(first.session_id);
    harness.race_continues(true);
    harness.restart();
    harness.set_status(first.session_id, SessionStatus::Failed);
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    let held = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(held.status, AttemptStatus::Running);
    assert_eq!(held.failure_class, None);
    assert!(held.preserved_commit.is_none());
    assert!(
        harness.continues().is_empty(),
        "the stale resume launched nothing"
    );
    assert_eq!(
        event_count(&harness, execution, "node_resume_refused").await,
        0
    );
    assert_eq!(
        event_count(&harness, execution, "node_resumed_after_restart").await,
        0
    );
    // The competing turn finishes the node; the attempt follows it.
    harness.race_continues(false);
    harness.commit_and_complete(first.session_id, "A");
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
}

/// A provider that cannot resume keeps the plan §3.4 outcome without asking.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn non_resumable_restart_cut_keeps_preserve_or_retry() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    harness.executor.advance(execution).await.unwrap();
    let first = harness.attempt(execution, "A", 0, 1).await;
    harness.restart();
    harness.set_status(first.session_id, SessionStatus::Failed);
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    assert!(harness.continues().is_empty());
    assert_eq!(
        harness.attempt(execution, "A", 0, 1).await.status,
        AttemptStatus::Interrupted
    );
    assert_eq!(
        event_count(&harness, execution, "node_resume_refused").await,
        0
    );
}

/// T2-A2: an attempt reserved but never launched relaunches after a restart
/// with exactly the reserved dedup key and pre-minted session id.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_a2_reserved_attempt_relaunches_with_same_dedup_key() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    let reserved = reserve_only(&harness, execution, "A").await;
    assert_eq!(reserved.status, AttemptStatus::Reserved);
    assert!(harness.launches().is_empty());

    harness.restart();
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    let launches = harness.launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].0, reserved.dedup_key);
    assert_eq!(launches[0].0, format!("topology.node:{execution}:A:0:1"));
    assert_eq!(launches[0].1, reserved.session_id);
    let running = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(running.status, AttemptStatus::Running);
    assert_eq!(running.boot_id, Some(harness.executor.boot_id));
}

/// T2-A3: recovery run twice (and a relaunch race) launches once; an
/// admitted launch without a session settles `lost` and is replaced by a new
/// uncharged attempt, never relaunched under the admitted key.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_a3_recovery_twice_launches_once() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    let reserved = reserve_only(&harness, execution, "A").await;
    for _ in 0..2 {
        harness.restart();
        recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
            .await
            .unwrap();
    }
    let launches = harness.launches();
    assert_eq!(launches.len(), 1, "two recovery passes launch once");
    assert_eq!(launches[0].0, reserved.dedup_key);

    // Admission recorded, then a crash before any session row: the key is
    // spent, so the attempt settles `lost` and attempt 2 launches instead.
    let other = harness.start(workflow(&["A"], &[])).await;
    let first = reserve_only(&harness, other, "A").await;
    {
        let store = harness.executor.store.lock().await;
        assert!(rows::mark_launching(&store, first.id, harness.executor.boot_id).unwrap());
        admit_invocation(&store, first.session_id, &first.dedup_key).unwrap();
    }
    harness.restart();
    // A busy host must not park lost-launch settlement or its retry behind
    // admission for new workers.
    harness
        .executor
        .effects
        .hold_launches
        .store(true, Ordering::SeqCst);
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    let first = harness.attempt(other, "A", 0, 1).await;
    assert_eq!(first.status, AttemptStatus::Lost);
    assert_eq!(
        first.failure_class.as_deref(),
        Some(rows::failure::LOST_BEFORE_SESSION)
    );
    let second = harness.attempt(other, "A", 0, 2).await;
    assert_eq!(second.status, AttemptStatus::Running);
    let keys: Vec<String> = harness
        .launches()
        .into_iter()
        .map(|launch| launch.0)
        .collect();
    assert!(keys.contains(&second.dedup_key));
    assert_eq!(
        keys.iter().filter(|key| **key == first.dedup_key).count(),
        0,
        "the admitted key never produces a session"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn host_load_does_not_hold_recovery_of_an_unadmitted_launching_attempt() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    let reserved = reserve_only(&harness, execution, "A").await;
    {
        let store = harness.executor.store.lock().await;
        rows::mark_launching(&store, reserved.id, harness.executor.boot_id).unwrap();
    }
    harness
        .executor
        .effects
        .hold_launches
        .store(true, Ordering::SeqCst);
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    assert_eq!(
        harness.attempt(execution, "A", 0, 1).await.status,
        AttemptStatus::Running
    );
    assert_eq!(harness.launches()[0].0, reserved.dedup_key);
    assert!(
        harness
            .executor
            .effects
            .launch_asks
            .lock()
            .unwrap()
            .is_empty(),
        "recovery bypasses new-work admission"
    );
}

/// #1641 S4b: a launch the sandbox capacity gate refuses (-32029) is a delay,
/// never a failure. The node stays `Reserved` (one attempt, no failure, no
/// session), one `admission_hold{kind}` event is written per transition (not
/// per tick), and the next tick launches it once the pressure clears.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn disk_floor_refusal_holds_node_reserved_then_launches() {
    let harness = Harness::new();
    let effects = Arc::clone(&harness.executor.effects);
    *effects.capacity_refusal.lock().unwrap() = Some("free_space_limit");
    let execution = harness.start(workflow(&["A"], &[])).await;

    for _ in 0..3 {
        assert_eq!(
            harness.executor.advance(execution).await.unwrap(),
            Step::Wait
        );
        let attempts = harness.attempts(execution).await;
        assert_eq!(attempts.len(), 1, "held, never failed or re-reserved");
        assert_eq!(attempts[0].status, AttemptStatus::Reserved);
        assert_eq!(attempts[0].failure_class, None);
        assert!(harness.launches().is_empty());
    }
    assert_eq!(
        event_count(&harness, execution, "admission_hold").await,
        1,
        "one hold event per transition, not per tick"
    );

    // The pressure changes kind: a new transition, a second event.
    *effects.capacity_refusal.lock().unwrap() = Some("source_root_limit");
    harness.executor.advance(execution).await.unwrap();
    harness.executor.advance(execution).await.unwrap();
    assert_eq!(event_count(&harness, execution, "admission_hold").await, 2);
    let kinds: Vec<String> = {
        let store = harness.executor.store.lock().await;
        let mut statement = store
            .conn
            .prepare(
                "SELECT json_extract(payload_json,'$.detail.kind') FROM topology_events \
                 WHERE execution_id=?1 AND kind='admission_hold' ORDER BY execution_seq",
            )
            .unwrap();
        statement
            .query_map([execution.to_string()], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    };
    assert_eq!(kinds, ["disk_floor", "source_root_limit"]);

    *effects.capacity_refusal.lock().unwrap() = None;
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let reserved = harness.attempt(execution, "A", 0, 1).await;
    assert_eq!(reserved.status, AttemptStatus::Running);
    let launches = harness.launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].0, reserved.dedup_key);
    assert_eq!(event_count(&harness, execution, "admission_hold").await, 2);
}

/// #1641 S4b: an operator execution's first launch starts when asked, but its
/// later nodes wait for the host like any other execution's.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn operator_execution_later_nodes_respect_host_load_hold() {
    let harness = Harness::new();
    let effects = Arc::clone(&harness.executor.effects);
    effects.hold_launches.store(true, Ordering::SeqCst);
    let execution = harness.start(workflow(&["A", "B"], &[("A", "B")])).await;

    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    assert_eq!(
        harness.attempt(execution, "A", 0, 1).await.status,
        AttemptStatus::Running,
        "the operator's first node is immediate even on a loaded host"
    );
    let a = harness.attempt(execution, "A", 0, 1).await;
    harness.commit_and_complete(a.session_id, "A-0-1");
    for _ in 0..3 {
        harness.executor.advance(execution).await.unwrap();
        let attempts = harness.attempts(execution).await;
        let b = attempts.iter().find(|attempt| attempt.node_id == "B");
        assert_eq!(
            b.map(|attempt| attempt.status),
            Some(AttemptStatus::Reserved),
            "the later node waits for the host: {attempts:?}"
        );
    }
    assert_eq!(harness.launches().len(), 1, "only A launched while held");
    assert!(
        effects
            .launch_asks
            .lock()
            .unwrap()
            .iter()
            .any(|(unattended, _)| *unattended),
        "the later node is asked as unattended"
    );

    effects.hold_launches.store(false, Ordering::SeqCst);
    harness.executor.advance(execution).await.unwrap();
    assert_eq!(harness.launches().len(), 2);
    assert_eq!(
        harness.attempt(execution, "B", 0, 1).await.status,
        AttemptStatus::Running
    );
}

/// #1641 S4b: a command node whose governor build slot is held stays
/// `Reserved` with one `admission_hold{disk_floor}` event, and starts once the
/// slot is granted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn command_node_build_slot_hold_keeps_it_reserved_then_runs() {
    let harness = Harness::new();
    let effects = Arc::clone(&harness.executor.effects);
    *effects.build_slot_hold.lock().unwrap() = Some("disk_floor");
    let execution = harness
        .start(with_steps(
            workflow(&["T"], &[]),
            &[("T", command_step("rsid"))],
            &[],
        ))
        .await;
    for _ in 0..3 {
        harness.executor.advance(execution).await.unwrap();
        let attempt = harness.attempt(execution, "T", 0, 1).await;
        assert_eq!(attempt.status, AttemptStatus::Reserved);
        assert_eq!(attempt.sandbox_root, None, "no sandbox while held");
    }
    assert_eq!(event_count(&harness, execution, "admission_hold").await, 1);
    *effects.build_slot_hold.lock().unwrap() = None;
    running_command(&harness, execution, "T").await;
}

/// Round 1 (`concurrent_test_does_not_race`): two advancers on separate
/// worker threads race for real. The first is suspended inside the
/// reservation window (ready set decided, transaction not yet run) while the
/// second is released against the same execution; each node still gets
/// exactly one attempt, one admission row and one launch.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2_concurrent_advancers_launch_once() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A", "B"], &[])).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let go = Arc::new(tokio::sync::Notify::new());
    *harness.executor.effects.reserve_hold.lock().unwrap() =
        Some((Arc::clone(&entered), Arc::clone(&go)));
    let first = harness.executor.clone();
    let first = tokio::spawn(async move { first.advance(execution).await });
    entered.notified().await;
    // The first advancer is parked inside the reservation window; the
    // second now runs on another worker thread against the same rows.
    let second = harness.executor.clone();
    let second = tokio::spawn(async move { second.advance(execution).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    go.notify_one();
    let (first, second) = (first.await.unwrap(), second.await.unwrap());
    assert_eq!((first.unwrap(), second.unwrap()), (Step::Wait, Step::Wait));
    assert_eq!(harness.launches().len(), 2, "one launch per node");
    let attempts = harness.attempts(execution).await;
    assert_eq!(attempts.len(), 2);
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt.attempt_no == 1 && attempt.status == AttemptStatus::Running)
    );
    let admissions: i64 = harness
        .executor
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT count(*) FROM model_invocations WHERE dedup_key LIKE ?1",
            [format!("topology.node:{execution}:%")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(admissions, 2, "exactly one admission row per node");
}

/// T2-A4: a session lost after it existed is replaced by a new attempt, and
/// the per-node bound (3) ends the execution.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_a4_loss_after_session_retries_within_bound() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    for attempt_no in 1..=3 {
        assert_eq!(
            harness.executor.advance(execution).await.unwrap(),
            Step::Wait
        );
        let attempt = harness.attempt(execution, "A", 0, attempt_no).await;
        assert_eq!(attempt.status, AttemptStatus::Running);
        harness
            .world
            .lock()
            .unwrap()
            .sessions
            .remove(&attempt.session_id);
    }
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    let attempts = harness.attempts(execution).await;
    assert_eq!(attempts.len(), 3);
    assert!(attempts.iter().all(|attempt| {
        attempt.failure_class.as_deref() == Some(rows::failure::LOST_AFTER_SESSION)
    }));
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Failed
    );
    assert_eq!(harness.launches().len(), 3);
}

/// T2-A5: interrupt is durable (`cancelling`), interrupts every node
/// session, reclaims each attempt's cache, and settles `cancelled`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_a5_interrupt_settles_cancelled() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A", "B"], &[])).await;
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    assert_eq!(harness.launches().len(), 2);
    assert_eq!(
        harness.executor.request_interrupt(execution).await.unwrap(),
        Some(rows::ExecutionStatus::Running)
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Cancelling
    );
    for _ in 0..4 {
        if harness.executor.advance(execution).await.unwrap() == Step::Done {
            break;
        }
    }
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Cancelled
    );
    let attempts = harness.attempts(execution).await;
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt.status == AttemptStatus::Cancelled)
    );
    {
        let world = harness.world.lock().unwrap();
        assert_eq!(world.interrupts.len(), 2);
        for attempt in &attempts {
            assert!(
                world.reclaims.contains(&attempt.session_id),
                "interrupt-path reclaim"
            );
        }
    }
    let store = harness.executor.store.lock().await;
    let snapshot = rows::execution_snapshot(&store, execution)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.status, WorkflowExecutionStatus::Interrupted);
    assert_eq!(
        snapshot
            .updates
            .iter()
            .filter(|update| update.finished)
            .map(|update| update.status)
            .collect::<Vec<_>>(),
        vec![WorkflowExecutionStatus::Interrupted],
        "exactly one finished update"
    );
}

/// Kill switch: with `topology_executor_enabled=false` nothing advances or
/// recovers; re-enabling resumes the same reserved attempt exactly once.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_kill_switch_pauses_all_effects() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    let reserved = reserve_only(&harness, execution, "A").await;
    harness.restart();
    harness
        .executor
        .effects
        .enabled
        .store(false, Ordering::SeqCst);
    let report = recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    assert!(report.advanced.is_empty());
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Paused
    );
    assert!(harness.launches().is_empty());
    assert_eq!(
        harness.attempt(execution, "A", 0, 1).await.status,
        AttemptStatus::Reserved
    );

    harness
        .executor
        .effects
        .enabled
        .store(true, Ordering::SeqCst);
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    let launches = harness.launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].0, reserved.dedup_key);
}

// ─── A7: TUI contract ───────────────────────────────────────────────────────

/// T2-A7: the durable projection round-trips the TUI/RPC contract: the
/// snapshot replays exactly the published updates in sequence, and a blocked
/// execution carries its CAS version and blocked attempt.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_a7_tui_contract_round_trips() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A", "B"], &[("A", "B")])).await;
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let snapshot = {
        let store = harness.executor.store.lock().await;
        rows::execution_snapshot(&store, execution)
            .unwrap()
            .unwrap()
    };
    let wire = serde_json::to_value(rsi_common::rpc::GetWorkflowExecutionResponse {
        lookup: WorkflowExecutionLookup::Found {
            execution: snapshot.clone(),
        },
    })
    .unwrap();
    let decoded: rsi_common::rpc::GetWorkflowExecutionResponse =
        serde_json::from_value(wire).unwrap();
    let WorkflowExecutionLookup::Found { execution: decoded } = decoded.lookup else {
        panic!("durable execution must be Found");
    };
    assert_eq!(decoded.status, WorkflowExecutionStatus::Succeeded);
    assert_eq!(decoded.workflow_name, "durable");
    assert_eq!(decoded.row_version, snapshot.row_version);
    let sequences: Vec<u64> = decoded
        .updates
        .iter()
        .map(|update| update.sequence)
        .collect();
    assert_eq!(sequences, (1..=sequences.len() as u64).collect::<Vec<_>>());
    assert_eq!(decoded.last_sequence, *sequences.last().unwrap());
    let published = harness.executor.effects.published.lock().unwrap().clone();
    let published: Vec<(u64, Option<String>, WorkflowExecutionStatus)> = published
        .into_iter()
        .map(|update| (update.sequence, update.node_id, update.status))
        .collect();
    let replayed: Vec<(u64, Option<String>, WorkflowExecutionStatus)> = decoded
        .updates
        .iter()
        .filter(|update| update.sequence > 1)
        .map(|update| (update.sequence, update.node_id.clone(), update.status))
        .collect();
    assert_eq!(published, replayed, "bus updates equal the durable replay");
    assert_eq!(
        decoded
            .updates
            .iter()
            .filter(|update| update.finished)
            .map(|update| update.status)
            .collect::<Vec<_>>(),
        vec![WorkflowExecutionStatus::Succeeded],
        "exactly one finished update (chain driver contract)"
    );
    assert!(
        decoded
            .updates
            .iter()
            .any(|update| update.node_id.as_deref() == Some("B")
                && update.node_state
                    == Some(rsi_common::types::WorkflowNodeExecutionState::Succeeded))
    );

    let blocked_execution = harness.start(workflow(&["A"], &[])).await;
    assert_eq!(
        harness.executor.advance(blocked_execution).await.unwrap(),
        Step::Wait
    );
    let attempt = harness.attempt(blocked_execution, "A", 0, 1).await;
    harness.commit_and_complete(attempt.session_id, "diverged");
    harness.set_status(attempt.session_id, SessionStatus::Interrupted);
    harness.executor.advance(blocked_execution).await.unwrap();
    let store = harness.executor.store.lock().await;
    let blocked = rows::execution_snapshot(&store, blocked_execution)
        .unwrap()
        .unwrap();
    assert_eq!(blocked.status, WorkflowExecutionStatus::Blocked);
    assert_eq!(blocked.blocked_attempt_id, Some(attempt.id));
    assert_eq!(blocked.blocked_reason.as_deref(), Some("preserved_work"));
    assert_eq!(
        blocked.row_version,
        Some(rows::current_row_version(&store, blocked_execution).unwrap())
    );
}

// ─── A9, A12, A13: preserved work ───────────────────────────────────────────

/// T2-A9: an interrupted diverged session blocks on preserved work; inspect,
/// a refused stale CAS, a refused confirmation on a non-discard action, and
/// accept (clean tree ⇒ `result_commit` = HEAD) resume the execution.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_a9_operator_resolve_preserved_work() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let running = harness.attempt(execution, "A", 0, 1).await;
    let head = harness.commit_and_complete(running.session_id, "work");
    let blocked = block_on_interrupt(&harness, execution).await;
    assert_eq!(blocked.preserved_commit.as_deref(), Some(head.as_str()));

    let stale = ResolveTopologyAttemptParams {
        execution_id: execution,
        attempt_id: blocked.id,
        action: TopologyAttemptAction::Accept,
        expected_row_version: harness.row_version(execution).await - 1,
        idempotency_key: "stale".into(),
        confirm_preserved_commit: None,
    };
    let error = harness.executor.resolve_attempt(&stale).await.unwrap_err();
    assert_eq!(error_code(&error), "stale_row_version");

    let error = harness
        .resolve(
            execution,
            blocked.id,
            TopologyAttemptAction::Accept,
            "confirm",
            Some(&head),
        )
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), "invalid_params");

    let inspected = harness
        .resolve(
            execution,
            blocked.id,
            TopologyAttemptAction::Inspect,
            "inspect-1",
            None,
        )
        .await
        .unwrap();
    let report = inspected.report.unwrap();
    assert_eq!(report.head.as_deref(), Some(head.as_str()));
    assert_eq!(report.base_commit, harness.base);
    assert_eq!(report.preserved_commit.as_deref(), Some(head.as_str()));
    assert!(report.diffstat.iter().any(|line| line.contains("work.txt")));

    let version = harness.row_version(execution).await;
    let accepted = harness
        .resolve_at(
            execution,
            blocked.id,
            TopologyAttemptAction::Accept,
            "accept-1",
            None,
            version,
        )
        .await
        .unwrap();
    assert_eq!(accepted.attempt.status, "succeeded");
    assert_eq!(accepted.attempt.resolution.as_deref(), Some("accepted"));
    assert_eq!(
        accepted.attempt.result_commit.as_deref(),
        Some(head.as_str())
    );
    // Idempotent replay returns the recorded outcome without a second write.
    let replay = harness
        .resolve_at(
            execution,
            blocked.id,
            TopologyAttemptAction::Accept,
            "accept-1",
            None,
            version,
        )
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
}

/// T2-A12: dirty interrupted custody is snapshotted byte-for-byte (tracked
/// edit plus untracked file) without touching the sandbox; a wrong discard is
/// refused, retry forks `preserved_commit`, and a confirmed discard deletes
/// the ref and is audited.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn t2_a12_retry_preserves_uncommitted_bytes() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let running = harness.attempt(execution, "A", 0, 1).await;
    let sandbox = harness.sandbox(running.session_id);
    std::fs::write(sandbox.join("file.txt"), "edited tracked bytes\n").unwrap();
    std::fs::write(sandbox.join("new.txt"), "untracked bytes\n").unwrap();
    let blocked = block_on_interrupt(&harness, execution).await;
    let preserved = blocked.preserved_commit.clone().unwrap();
    let preserved_ref = blocked.preserved_ref.clone().unwrap();
    assert_eq!(
        preserved_ref,
        format!("refs/rsi/topology-preserved/{execution}/A/0/1")
    );
    assert_eq!(
        git(&harness.repo, &["rev-parse", &preserved_ref]),
        preserved
    );
    assert_eq!(
        git(&harness.repo, &["show", &format!("{preserved}:file.txt")]),
        "edited tracked bytes"
    );
    assert_eq!(
        git(&harness.repo, &["show", &format!("{preserved}:new.txt")]),
        "untracked bytes"
    );
    assert_eq!(
        git(&harness.repo, &["rev-parse", &format!("{preserved}^")]),
        harness.base
    );
    // The sandbox index and worktree are untouched.
    let status = git(
        &sandbox,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    assert!(
        status.contains("M file.txt") && status.contains("?? new.txt"),
        "{status}"
    );

    let wrong = "0".repeat(40);
    let error = harness
        .resolve(
            execution,
            blocked.id,
            TopologyAttemptAction::Discard,
            "discard-wrong",
            Some(&wrong),
        )
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), "preserved_commit_mismatch");
    let error = harness
        .resolve(
            execution,
            blocked.id,
            TopologyAttemptAction::Discard,
            "discard-missing",
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), "invalid_params");
    assert_eq!(
        git(&harness.repo, &["rev-parse", &preserved_ref]),
        preserved
    );

    let retried = harness
        .resolve(
            execution,
            blocked.id,
            TopologyAttemptAction::Retry,
            "retry-1",
            None,
        )
        .await
        .unwrap();
    assert_eq!(retried.attempt.resolution.as_deref(), Some("retried"));
    let second = harness.attempt(execution, "A", 0, 2).await;
    assert_eq!(second.base_commit, preserved);
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let launch = harness.launches().pop().unwrap();
    assert_eq!(
        launch.2, preserved,
        "attempt 2 forks the preservation point"
    );
    let new_sandbox = harness.sandbox(second.session_id);
    assert_eq!(
        std::fs::read_to_string(new_sandbox.join("new.txt")).unwrap(),
        "untracked bytes\n"
    );
    // Retry keeps the old sandbox and ref.
    assert_eq!(
        std::fs::read_to_string(sandbox.join("new.txt")).unwrap(),
        "untracked bytes\n"
    );
    assert_eq!(
        git(&harness.repo, &["rev-parse", &preserved_ref]),
        preserved
    );

    // Attempt 2 is interrupted dirty too; this time the operator discards.
    std::fs::write(new_sandbox.join("again.txt"), "more\n").unwrap();
    harness.set_status(second.session_id, SessionStatus::Interrupted);
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    let second = harness.attempt(execution, "A", 0, 2).await;
    let second_commit = second.preserved_commit.clone().unwrap();
    let second_ref = second.preserved_ref.clone().unwrap();
    let discarded = harness
        .resolve(
            execution,
            second.id,
            TopologyAttemptAction::Discard,
            "discard-1",
            Some(&second_commit),
        )
        .await
        .unwrap();
    assert_eq!(discarded.attempt.resolution.as_deref(), Some("discarded"));
    assert!(
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &second_ref])
            .current_dir(&harness.repo)
            .status()
            .map(|status| !status.success())
            .unwrap(),
        "the discarded preservation ref is deleted"
    );
    assert!(
        harness
            .world
            .lock()
            .unwrap()
            .released
            .contains(&second.session_id)
    );
    let store = harness.executor.store.lock().await;
    let (actor, commit): (String, String) = store
        .conn
        .query_row(
            "SELECT actor_kind,json_extract(payload_json,'$.detail.preserved_commit') FROM topology_events \
             WHERE execution_id=?1 AND kind='preserved_work_discarded'",
            [execution.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (actor.as_str(), commit.as_str()),
        ("operator", second_commit.as_str())
    );
    drop(store);
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Failed
    );
}

/// T2-A13: a clean diverged attempt needs no snapshot: its HEAD is pinned
/// create-only and verified; retry forks that HEAD, and a moved ref refuses
/// retry with `preservation_point_unverified`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_a13_clean_diverged_retry_forks_verified_pin() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let running = harness.attempt(execution, "A", 0, 1).await;
    let head = harness.commit_and_complete(running.session_id, "clean");
    let blocked = block_on_interrupt(&harness, execution).await;
    let preserved_ref = blocked.preserved_ref.clone().unwrap();
    assert_eq!(
        blocked.preserved_commit.as_deref(),
        Some(head.as_str()),
        "no snapshot commit"
    );
    assert_eq!(git(&harness.repo, &["rev-parse", &preserved_ref]), head);
    let digest: Option<String> = {
        let store = harness.executor.store.lock().await;
        store
            .conn
            .query_row(
                "SELECT preserved_paths_digest FROM topology_node_attempts WHERE id=?1",
                [blocked.id.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(digest, None);

    git(
        &harness.repo,
        &["update-ref", &preserved_ref, &harness.base],
    );
    let error = harness
        .resolve(
            execution,
            blocked.id,
            TopologyAttemptAction::Retry,
            "retry-moved",
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error_code(&error), "preservation_point_unverified");
    git(&harness.repo, &["update-ref", &preserved_ref, &head]);

    harness
        .resolve(
            execution,
            blocked.id,
            TopologyAttemptAction::Retry,
            "retry-ok",
            None,
        )
        .await
        .unwrap();
    let second = harness.attempt(execution, "A", 0, 2).await;
    assert_eq!(second.base_commit, head);
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let new_sandbox = harness.sandbox(second.session_id);
    assert_eq!(git(&new_sandbox, &["rev-parse", "HEAD"]), head);
}

// ─── A10, A14: loop lineage ─────────────────────────────────────────────────

/// T2-A10: A→B→C with back-edge C→B, two iterations and a restart between
/// them: B@1 forks C@0's persisted pin and A stays an ancestor of C@1.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_a10_two_iteration_loop_lineage() {
    let mut harness = Harness::new();
    let definition = with_loops(
        workflow(&["A", "B", "C"], &[("A", "B"), ("B", "C")]),
        &[("C", "B")],
        &[&["B", "C"]],
        &UntilCondition::MaxIterations(2),
    );
    let execution = harness.start(definition).await;
    // Iteration 0: A, B@0, C@0.
    loop {
        assert_eq!(
            harness.executor.advance(execution).await.unwrap(),
            Step::Wait
        );
        let attempts = harness.attempts(execution).await;
        if attempts
            .iter()
            .any(|attempt| attempt.node_id == "B" && attempt.iteration == 1)
        {
            break;
        }
        for attempt in attempts
            .iter()
            .filter(|attempt| attempt.status == AttemptStatus::Running)
        {
            harness.commit_and_complete(
                attempt.session_id,
                &format!("{}{}", attempt.node_id, attempt.iteration),
            );
        }
    }
    // Restart between the iterations, while B@1 is in flight.
    harness.restart();
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );

    let result = |attempts: &[AttemptRow], node: &str, iteration: u32| {
        attempts
            .iter()
            .find(|attempt| {
                attempt.node_id == node
                    && attempt.iteration == iteration
                    && attempt.status == AttemptStatus::Succeeded
            })
            .unwrap()
            .clone()
    };
    let attempts = harness.attempts(execution).await;
    let (a, b0, c0, b1, c1) = (
        result(&attempts, "A", 0),
        result(&attempts, "B", 0),
        result(&attempts, "C", 0),
        result(&attempts, "B", 1),
        result(&attempts, "C", 1),
    );
    assert_eq!(b0.base_commit, a.result_commit.as_deref().unwrap());
    assert_eq!(c0.base_commit, b0.result_commit.as_deref().unwrap());
    assert_eq!(
        b1.base_commit,
        c0.result_commit.as_deref().unwrap(),
        "B@1 forks C@0's pin"
    );
    assert_eq!(c1.base_commit, b1.result_commit.as_deref().unwrap());
    git(
        &harness.repo,
        &[
            "merge-base",
            "--is-ancestor",
            a.result_commit.as_deref().unwrap(),
            c1.result_commit.as_deref().unwrap(),
        ],
    );
    assert!(
        !attempts.iter().any(|attempt| attempt.iteration > 1),
        "two iterations only"
    );
    // Success releases every node pin, in Git and in the rows.
    let pinned: i64 = harness
        .executor
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT count(*) FROM topology_node_attempts WHERE execution_id=?1 AND pin_ref IS NOT NULL",
            [execution.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pinned, 0);
    assert_eq!(
        git(
            &harness.repo,
            &["for-each-ref", &format!("refs/rsi/topology/{execution}")]
        ),
        ""
    );
}

fn two_regions(
    until: &UntilCondition,
    x_cap: Option<usize>,
    y2_from_x2: bool,
) -> WorkflowDefinition {
    let mut definition = with_loops(
        workflow(
            &["X1", "X2", "Y1", "Y2"],
            &[("X1", "X2"), ("X2", "Y1"), ("Y1", "Y2")],
        ),
        &[("X2", "X1"), ("Y2", "Y1")],
        &[&["X1", "X2"], &["Y1", "Y2"]],
        until,
    );
    for node in &mut definition.nodes {
        if let Some(cap) = x_cap
            && node.id.starts_with('X')
        {
            node.repeat_policy = Some(RepeatPolicy {
                max_iterations: cap,
                termination: None,
            });
        }
        if y2_from_x2 && node.id == "Y2" {
            node.tags.push("custody.from=node:X2".into());
        }
    }
    definition
}

/// T2-A14 (R4-3): a forward edge between two loop regions is an entry edge:
/// Y's iteration 0 forks X's final iteration, and a cross-region
/// `custody.from` never asks for a nonexistent same-iteration pin.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_a14_cross_region_forward_edge_forks_final_iteration() {
    // (a) X and Y both run two iterations.
    let harness = Harness::new();
    let execution = harness
        .start(two_regions(&UntilCondition::MaxIterations(2), None, false))
        .await;
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
    let attempts = harness.attempts(execution).await;
    let get = |node: &str, iteration: u32| {
        attempts
            .iter()
            .find(|attempt| attempt.node_id == node && attempt.iteration == iteration)
            .unwrap_or_else(|| panic!("{node}@{iteration}"))
    };
    assert_eq!(
        get("Y1", 0).base_commit,
        get("X2", 1).result_commit.clone().unwrap(),
        "Y1@0 forks X2@1"
    );
    assert_ne!(
        get("Y1", 0).base_commit,
        get("X2", 0).result_commit.clone().unwrap()
    );
    assert_eq!(
        get("Y1", 1).base_commit,
        get("Y2", 0).result_commit.clone().unwrap(),
        "Y1@1 forks Y2@0"
    );

    // (b) X runs once, Y twice, and Y2 names X2 explicitly.
    let execution = harness
        .start(two_regions(
            &UntilCondition::MaxIterations(2),
            Some(1),
            true,
        ))
        .await;
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
    let attempts = harness.attempts(execution).await;
    let get = |node: &str, iteration: u32| {
        attempts
            .iter()
            .find(|attempt| attempt.node_id == node && attempt.iteration == iteration)
            .unwrap_or_else(|| panic!("{node}@{iteration}"))
    };
    assert!(
        !attempts
            .iter()
            .any(|attempt| attempt.node_id.starts_with('X') && attempt.iteration > 0)
    );
    assert_eq!(
        get("Y2", 1).base_commit,
        get("X2", 0).result_commit.clone().unwrap(),
        "Y2@1 forks X2 final"
    );
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt.failure_class.is_none())
    );
}

// ─── Review round 1 regressions ─────────────────────────────────────────────

async fn event_count(harness: &Harness, execution: Uuid, kind: &str) -> i64 {
    harness
        .executor
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT count(*) FROM topology_events WHERE execution_id=?1 AND kind=?2",
            rusqlite::params![execution.to_string(), kind],
            |row| row.get(0),
        )
        .unwrap()
}

fn ref_exists(repo: &Path, name: &str) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", name])
        .current_dir(repo)
        .status()
        .unwrap()
        .success()
}

/// Launch nodes, then interrupt each node's session with committed (diverged)
/// work so every attempt blocks on preserved work.
async fn block_all(harness: &Harness, execution: Uuid) -> Vec<AttemptRow> {
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let running = harness.attempts(execution).await;
    for attempt in &running {
        harness.commit_and_complete(attempt.session_id, &format!("work-{}", attempt.node_id));
        harness.set_status(attempt.session_id, SessionStatus::Interrupted);
    }
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    harness.attempts(execution).await
}

/// Round 1 (`resolution_key_not_request_bound`): the idempotency key binds
/// the whole request. The same key and fingerprint replay; the same key and
/// action for another attempt or another CAS version conflict.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_r1_resolution_key_is_request_bound() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A", "B"], &[])).await;
    let blocked = block_all(&harness, execution).await;
    assert!(
        blocked
            .iter()
            .all(|attempt| attempt.status == AttemptStatus::Blocked)
    );
    let (a, b) = (&blocked[0], &blocked[1]);
    let version = harness.row_version(execution).await;
    let accepted = harness
        .resolve_at(
            execution,
            a.id,
            TopologyAttemptAction::Accept,
            "k",
            None,
            version,
        )
        .await
        .unwrap();
    assert!(!accepted.deduplicated);
    let replay = harness
        .resolve_at(
            execution,
            a.id,
            TopologyAttemptAction::Accept,
            "k",
            None,
            version,
        )
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.attempt.attempt_id, a.id);

    let other_attempt = harness
        .resolve(execution, b.id, TopologyAttemptAction::Accept, "k", None)
        .await
        .unwrap_err();
    assert_eq!(error_code(&other_attempt), "idempotency_conflict");
    let other_version = harness
        .resolve_at(
            execution,
            a.id,
            TopologyAttemptAction::Accept,
            "k",
            None,
            version + 1,
        )
        .await
        .unwrap_err();
    assert_eq!(error_code(&other_version), "idempotency_conflict");
    // B is still blocked: the conflicting request resolved nothing.
    assert_eq!(
        harness.attempt(execution, "B", 0, 1).await.status,
        AttemptStatus::Blocked
    );
    let fresh = harness
        .resolve(execution, b.id, TopologyAttemptAction::Accept, "k2", None)
        .await
        .unwrap();
    assert_eq!(fresh.attempt.resolution.as_deref(), Some("accepted"));
}

/// Round 1 (`preservation_crash_not_idempotent`): a crash after the
/// create-only preservation ref exists but before the attempt records it is
/// replayed as the same preservation point, for dirty and clean custody.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_r1_preservation_crash_is_idempotent() {
    for dirty in [true, false] {
        let mut harness = Harness::new();
        let execution = harness.start(workflow(&["A"], &[])).await;
        assert_eq!(
            harness.executor.advance(execution).await.unwrap(),
            Step::Wait
        );
        let attempt = harness.attempt(execution, "A", 0, 1).await;
        let sandbox = harness.sandbox(attempt.session_id);
        if dirty {
            std::fs::write(sandbox.join("partial.txt"), "unsaved\n").unwrap();
        } else {
            harness.commit_and_complete(attempt.session_id, "clean");
        }
        harness.set_status(attempt.session_id, SessionStatus::Interrupted);
        // The first incarnation created the ref, then died before settling.
        let name = crate::topology::custody::preserved_ref(execution, "A", 0, 1);
        let first = crate::topology::custody::preserve_sandbox(
            &sandbox,
            &name,
            attempt.id,
            &attempt.base_commit,
        )
        .unwrap()
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        harness.restart();
        recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
            .await
            .unwrap();
        let blocked = harness.attempt(execution, "A", 0, 1).await;
        assert_eq!(blocked.status, AttemptStatus::Blocked, "dirty={dirty}");
        assert_eq!(
            blocked.failure_class.as_deref(),
            Some(rows::failure::PRESERVED_WORK)
        );
        assert_eq!(
            blocked.preserved_commit.as_deref(),
            Some(first.commit.as_str())
        );
        assert_eq!(git(&harness.repo, &["rev-parse", &name]), first.commit);
    }
}

fn with_failure_policy(
    mut workflow: WorkflowDefinition,
    node: &str,
    policy: &str,
) -> WorkflowDefinition {
    workflow.metadata.insert(
        "failure_policies".into(),
        GraphValue::String(serde_json::json!({ node: policy }).to_string()),
    );
    workflow
}

/// Round 1 (`ordinary_retry_regression`): `FailurePolicy::Retry` keeps the
/// legacy budget exactly: `repeat_policy.max_iterations` retries (5 ⇒ six
/// launches), and the legacy runner reads the same budget function, so the
/// kill switch cannot change it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_r1_retry_budget_matches_legacy_runner() {
    let harness = Harness::new();
    let mut definition = with_failure_policy(workflow(&["A"], &[]), "A", "Retry");
    definition.nodes[0].repeat_policy = Some(RepeatPolicy {
        max_iterations: 5,
        termination: None,
    });
    assert_eq!(
        crate::session::graph_runner::failure_retry_budget(&definition.nodes[0]),
        5
    );
    let execution = harness.start(definition).await;
    for attempt_no in 1..=6 {
        assert_eq!(
            harness.executor.advance(execution).await.unwrap(),
            Step::Wait
        );
        let attempt = harness.attempt(execution, "A", 0, attempt_no).await;
        harness.set_status(attempt.session_id, SessionStatus::Failed);
    }
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    assert_eq!(harness.launches().len(), 6, "one run plus five retries");
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Failed
    );
    // The legacy (kill-switch) runner draws its budget from the same function.
    let legacy = include_str!("../session/graph_runner.rs");
    assert!(legacy.contains("let max_retries = failure_retry_budget(node_def);"));
}

/// Round 1 (`loop_cap_without_until`): a loop guarded only by node
/// `max_iterations` runs to that cap.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_r1_cap_only_loop_runs_to_node_cap() {
    let harness = Harness::new();
    let mut definition = workflow(&["A", "B", "C"], &[("A", "B"), ("B", "C"), ("C", "B")]);
    definition.metadata.insert(
        "loop_edges".into(),
        GraphValue::String(r#"[{"from":"C","to":"B"}]"#.into()),
    );
    definition.metadata.insert(
        "scc_regions".into(),
        GraphValue::String(r#"[["B","C"]]"#.into()),
    );
    for node in definition.nodes.iter_mut().filter(|node| node.id != "A") {
        node.repeat_policy = Some(RepeatPolicy {
            max_iterations: 5,
            termination: None,
        });
    }
    let execution = harness.start(definition).await;
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
    let iterations: Vec<u32> = harness
        .attempts(execution)
        .await
        .iter()
        .filter(|attempt| attempt.node_id == "B")
        .map(|attempt| attempt.iteration)
        .collect();
    assert_eq!(iterations, vec![0, 1, 2, 3, 4]);
}

/// Round 1 (`large_output_lost`): an output above 64 KiB is stored as
/// path@commit, is read back and digest-verified by the next daemon
/// incarnation, and reaches a downstream prompt planned after the restart in
/// full.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_r1_large_output_round_trips_through_git() {
    let mut harness = Harness::new();
    let mut definition = workflow(&["A", "B", "C"], &[("A", "B"), ("B", "C"), ("A", "C")]);
    definition.nodes[2].tags.push("custody.from=node:B".into());
    // T3a: the full last message reaches C only because C asks for it.
    let definition = with_steps(
        definition,
        &[(
            "C",
            serde_json::json!({"kind": "session", "pass_content": true}),
        )],
        &[],
    );
    let execution = harness.start(definition).await;
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let a = harness.attempt(execution, "A", 0, 1).await;
    harness.commit_and_complete(a.session_id, "A");
    // A valid strict handoff followed by a >64 KiB body (T3a strict success).
    let large = format!(
        "PIPELINE HANDOFF — RESEARCH:\ndoc_path: /tmp/A.md\nstatus: complete\n{}END-OF-LARGE-OUTPUT",
        "x".repeat(70 * 1024)
    );
    harness
        .world
        .lock()
        .unwrap()
        .sessions
        .get_mut(&a.session_id)
        .unwrap()
        .content
        .clone_from(&large);
    // Boot 1 settles A (externalizing its output) and launches B.
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let settled = harness.attempt(execution, "A", 0, 1).await;
    let external =
        &settled.output.as_ref().unwrap()[crate::topology::executor::EXTERNAL_OUTPUT_KEY];
    assert!(external["size"].as_u64().unwrap() > 64 * 1024);
    assert!(ref_exists(&harness.repo, external["ref"].as_str().unwrap()));
    let b = harness.attempt(execution, "B", 0, 1).await;
    harness.commit_and_complete(b.session_id, "B");

    // Boot 2 plans C from A's stored output.
    harness.restart();
    recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    let c = harness.attempt(execution, "C", 0, 1).await;
    assert!(c.query().contains("END-OF-LARGE-OUTPUT"));
    assert!(c.query().len() > 64 * 1024);
    let read = crate::topology::executor::node_output(&harness.repo, &settled).unwrap();
    assert_eq!(read.get("content"), Some(&GraphValue::String(large)));
    // A tampered digest is refused, never silently empty.
    let mut tampered = settled.clone();
    tampered.output.as_mut().unwrap()[crate::topology::executor::EXTERNAL_OUTPUT_KEY]["digest"] =
        serde_json::json!("sha256:0");
    assert!(crate::topology::executor::node_output(&harness.repo, &tampered).is_err());
}

/// Round 1 (`discard_cleanup_not_recovered`): a discard is two-phase. An
/// effect failure after the discard is recorded leaves it pending (the
/// execution stays blocked); recovery or a replay of the same request
/// finishes it, and only then does the execution resume.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_r1_discard_completes_after_crash() {
    let mut harness = Harness::new();
    let by_recovery = harness.start(workflow(&["A"], &[])).await;
    let by_replay = harness.start(workflow(&["A"], &[])).await;
    for execution in [by_recovery, by_replay] {
        let blocked = block_all(&harness, execution).await.remove(0);
        let commit = blocked.preserved_commit.clone().unwrap();
        let version = harness.row_version(execution).await;
        harness
            .executor
            .effects
            .fail_release_once
            .store(true, Ordering::SeqCst);
        let error = harness
            .resolve_at(
                execution,
                blocked.id,
                TopologyAttemptAction::Discard,
                "discard",
                Some(&commit),
                version,
            )
            .await;
        assert!(error.is_err(), "phase 2 failed");
        assert_eq!(
            harness.status(execution).await,
            rows::ExecutionStatus::Blocked
        );
        assert_eq!(
            event_count(&harness, execution, "preserved_work_discarded").await,
            1
        );
        assert_eq!(
            event_count(&harness, execution, "preserved_work_discard_completed").await,
            0
        );
        if execution == by_recovery {
            harness.restart();
            let report =
                recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
                    .await
                    .unwrap();
            assert_eq!(report.discards_completed, 1);
        } else {
            let replay = harness
                .resolve_at(
                    execution,
                    blocked.id,
                    TopologyAttemptAction::Discard,
                    "discard",
                    Some(&commit),
                    version,
                )
                .await
                .unwrap();
            assert!(replay.deduplicated);
        }
        assert_eq!(
            event_count(&harness, execution, "preserved_work_discard_completed").await,
            1
        );
        assert!(!ref_exists(
            &harness.repo,
            blocked.preserved_ref.as_deref().unwrap()
        ));
        assert!(
            harness
                .world
                .lock()
                .unwrap()
                .released
                .contains(&blocked.session_id)
        );
        assert_eq!(
            harness.executor.advance(execution).await.unwrap(),
            Step::Done
        );
        assert_eq!(
            harness.status(execution).await,
            rows::ExecutionStatus::Failed
        );
    }
}

/// Round 1 (`terminal_sessions_not_archived`): success and cancellation
/// archive node sessions at settlement; failure preserves them until the
/// forensic TTL; a preserved-work sandbox is never archived.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_r1_settlement_archives_node_sessions() {
    let harness = Harness::new();
    let succeeded = harness.start(workflow(&["A", "B"], &[("A", "B")])).await;
    assert_eq!(harness.run_to_end(succeeded).await, Step::Done);
    let sessions: Vec<Uuid> = harness
        .attempts(succeeded)
        .await
        .iter()
        .map(|attempt| attempt.session_id)
        .collect();
    for session in &sessions {
        assert!(harness.world.lock().unwrap().released.contains(session));
    }
    assert_eq!(
        event_count(&harness, succeeded, "sessions_released").await,
        1
    );

    let failed = harness.start(workflow(&["A"], &[])).await;
    assert_eq!(harness.executor.advance(failed).await.unwrap(), Step::Wait);
    let lost = harness.attempt(failed, "A", 0, 1).await;
    harness.set_status(lost.session_id, SessionStatus::Failed);
    assert_eq!(harness.executor.advance(failed).await.unwrap(), Step::Done);
    assert_eq!(harness.status(failed).await, rows::ExecutionStatus::Failed);
    crate::topology::recovery::settlement_cleanup(&harness.executor, 64).await;
    assert!(
        !harness
            .world
            .lock()
            .unwrap()
            .released
            .contains(&lost.session_id)
    );
    {
        let store = harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE topology_executions SET finished_at='2020-01-01T00:00:00.000000000Z' WHERE id=?1",
                [failed.to_string()],
            )
            .unwrap();
    }
    crate::topology::recovery::settlement_cleanup(&harness.executor, 64).await;
    assert!(
        harness
            .world
            .lock()
            .unwrap()
            .released
            .contains(&lost.session_id)
    );

    let cancelled = harness.start(workflow(&["A", "B"], &[])).await;
    let blocked = block_all(&harness, cancelled).await;
    harness
        .resolve(
            cancelled,
            blocked[1].id,
            TopologyAttemptAction::Accept,
            "accept-b",
            None,
        )
        .await
        .unwrap();
    harness.executor.request_interrupt(cancelled).await.unwrap();
    harness.executor.advance(cancelled).await.unwrap();
    assert_eq!(
        harness.status(cancelled).await,
        rows::ExecutionStatus::Cancelled
    );
    crate::topology::recovery::settlement_cleanup(&harness.executor, 64).await;
    let world = harness.world.lock().unwrap();
    assert!(
        !world.released.contains(&blocked[0].session_id),
        "a preserved-work sandbox is kept until discard"
    );
    assert!(
        !world.released.contains(&blocked[1].session_id),
        "an accepted preservation point keeps its sandbox too"
    );
}

/// Round 1 (`success_pin_cleanup_crash_gap`): a crash after `succeeded` but
/// before the pins were released is repaired by the next recovery pass.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_r1_success_pins_released_after_crash() {
    let mut harness = Harness::new();
    let execution = harness.start(workflow(&["A"], &[])).await;
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let attempt = harness.attempt(execution, "A", 0, 1).await;
    let pin = crate::topology::custody::node_pin_ref(execution, "A", 0);
    // Reconstruct the exact crash state: status succeeded, pin and session
    // still held, no cleanup recorded.
    git(
        &harness.repo,
        &[
            "update-ref",
            &pin,
            attempt.result_commit.as_deref().unwrap(),
        ],
    );
    {
        let store = harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE topology_node_attempts SET pin_ref=?2 WHERE id=?1",
                rusqlite::params![attempt.id.to_string(), pin],
            )
            .unwrap();
        store
            .conn
            .execute(
                "DELETE FROM topology_events WHERE execution_id=?1 AND kind IN ('pins_released','sessions_released')",
                [execution.to_string()],
            )
            .unwrap();
    }
    harness.world.lock().unwrap().released.clear();
    harness.restart();
    let report = recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(report.cleanup_steps, 2);
    assert!(!ref_exists(&harness.repo, &pin));
    assert_eq!(event_count(&harness, execution, "pins_released").await, 1);
    assert!(
        harness
            .world
            .lock()
            .unwrap()
            .released
            .contains(&attempt.session_id)
    );
    // Idempotent: a second pass finds nothing owed.
    let again = recover_after_restart(&harness.executor, 8, std::time::Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(again.cleanup_steps, 0);
}

// ─── Review round 2 regressions ─────────────────────────────────────────────

/// Round 2 (`resolution_key_concurrent_inspect`): two requests with one
/// idempotency key race for real. The first passes the key pre-check and is
/// parked before its recording transaction while the second records; the
/// first then sees the key inside its transaction: a different request is an
/// `idempotency_conflict`, an identical one a replay. Exactly one event per key.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2_r2_concurrent_resolution_key_records_once() {
    let harness = Harness::new();
    let execution = harness.start(workflow(&["A", "B"], &[])).await;
    let blocked = block_all(&harness, execution).await;
    let (a, b) = (blocked[0].id, blocked[1].id);
    let version = harness.row_version(execution).await;
    let executor = harness.executor.clone();
    let request = move |attempt_id: Uuid, key: &str| ResolveTopologyAttemptParams {
        execution_id: execution,
        attempt_id,
        action: TopologyAttemptAction::Inspect,
        expected_row_version: version,
        idempotency_key: key.to_owned(),
        confirm_preserved_commit: None,
    };
    let key_events = |key: &'static str| {
        let executor = executor.clone();
        async move {
            executor
                .store
                .lock()
                .await
                .conn
                .query_row(
                    "SELECT count(*) FROM topology_events WHERE execution_id=?1 \
                     AND json_extract(payload_json,'$.detail.idempotency_key')=?2",
                    rusqlite::params![execution.to_string(), key],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
        }
    };
    for (key, second_attempt, same_request) in [("k-conflict", b, false), ("k-replay", a, true)] {
        let entered = Arc::new(tokio::sync::Notify::new());
        let go = Arc::new(tokio::sync::Notify::new());
        *harness.executor.effects.record_hold.lock().unwrap() =
            Some((Arc::clone(&entered), Arc::clone(&go)));
        let first = {
            let executor = harness.executor.clone();
            let params = request(a, key);
            tokio::spawn(async move { executor.resolve_attempt(&params).await })
        };
        entered.notified().await;
        let second = {
            let executor = harness.executor.clone();
            let params = request(second_attempt, key);
            tokio::spawn(async move { executor.resolve_attempt(&params).await })
        };
        let second = second.await.unwrap().unwrap();
        assert!(!second.deduplicated, "the second request records first");
        go.notify_one();
        let first = first.await.unwrap();
        if same_request {
            let first = first.unwrap();
            assert!(first.deduplicated, "an identical racing request replays");
        } else {
            assert_eq!(error_code(&first.unwrap_err()), "idempotency_conflict");
        }
        assert_eq!(key_events(key).await, 1, "exactly one event for {key}");
    }
}

/// Fail every node attempt until it has run `succeed_at` times, then let it
/// succeed (`None`: never succeed).
async fn run_with_failures(harness: &Harness, execution: Uuid, succeed_at: Option<u32>) -> Step {
    for _ in 0..400 {
        let step = harness.executor.advance(execution).await.unwrap();
        if step != Step::Wait {
            return step;
        }
        for attempt in harness.attempts(execution).await {
            if attempt.status != AttemptStatus::Running {
                continue;
            }
            if succeed_at == Some(attempt.attempt_no) {
                harness.commit_and_complete(attempt.session_id, &format!("{}-ok", attempt.node_id));
            } else {
                harness.set_status(attempt.session_id, SessionStatus::Failed);
            }
        }
    }
    panic!("execution did not settle");
}

/// Round 2 (`ordinary_retry_global_cap`): the execution cap derives from the
/// legacy budget, so parity holds above 64 attempts: one Retry node with 65
/// retries launches 66 times; two sequential Retry nodes with 32 retries each
/// succeed after 66 launches. The kill-switch runner uses the same budget
/// function (source-level parity, as in round 1).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t2_r2_legacy_retry_budget_above_default_cap() {
    let retrying = |nodes: &[&str], edges: &[(&str, &str)], budget: usize| {
        let mut definition = workflow(nodes, edges);
        let policies: serde_json::Map<String, serde_json::Value> = nodes
            .iter()
            .map(|node| ((*node).to_owned(), serde_json::json!("Retry")))
            .collect();
        definition.metadata.insert(
            "failure_policies".into(),
            GraphValue::String(serde_json::Value::Object(policies).to_string()),
        );
        for node in &mut definition.nodes {
            node.repeat_policy = Some(RepeatPolicy {
                max_iterations: budget,
                termination: None,
            });
        }
        definition
    };

    let harness = Harness::new();
    let single = harness.start(retrying(&["A"], &[], 65)).await;
    assert_eq!(run_with_failures(&harness, single, None).await, Step::Done);
    assert_eq!(harness.status(single).await, rows::ExecutionStatus::Failed);
    assert_eq!(
        harness.attempts(single).await.len(),
        66,
        "one run plus 65 retries"
    );

    let launched_before = harness.launches().len();
    let pair = harness
        .start(retrying(&["A", "B"], &[("A", "B")], 32))
        .await;
    assert_eq!(
        run_with_failures(&harness, pair, Some(33)).await,
        Step::Done
    );
    assert_eq!(harness.status(pair).await, rows::ExecutionStatus::Succeeded);
    assert_eq!(harness.launches().len() - launched_before, 66);

    let legacy = include_str!("../session/graph_runner.rs");
    assert!(legacy.contains("let max_retries = failure_retry_budget(node_def);"));
}

/// T2 round-2 integration: a typed topology (any `params.step`) is bounded
/// by plan §2.5, not the legacy parity derivation. The execution cap is the
/// stored `max_node_attempts` (64); a node's `max_attempts` is 1 by default,
/// and 3 at most under `Retry`. A legacy shape keeps its derived cap
/// (`t2_r2_legacy_retry_budget_above_default_cap`).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn t3a_typed_topology_uses_plan_attempt_cap() {
    use crate::topology::graph::GraphShape;
    let retrying = |nodes: &[&str], budget: usize| {
        let mut definition = workflow(nodes, &[]);
        let policies: serde_json::Map<String, serde_json::Value> = nodes
            .iter()
            .map(|node| ((*node).to_owned(), serde_json::json!("Retry")))
            .collect();
        definition.metadata.insert(
            "failure_policies".into(),
            GraphValue::String(serde_json::Value::Object(policies).to_string()),
        );
        for node in &mut definition.nodes {
            node.repeat_policy = Some(RepeatPolicy {
                max_iterations: budget,
                termination: None,
            });
        }
        definition
    };
    let typed = |definition: WorkflowDefinition, nodes: &[&str]| {
        let steps: Vec<(&str, serde_json::Value)> = nodes
            .iter()
            .map(|node| (*node, serde_json::json!({"kind": "session"})))
            .collect();
        with_steps(definition, &steps, &[])
    };

    // Shape: the legacy derivation exceeds 64; the typed shape uses 64.
    let legacy_shape = GraphShape::from_workflow(&retrying(&["A"], 65)).unwrap();
    assert_eq!(legacy_shape.attempt_cap(64), 66);
    assert_eq!(legacy_shape.retry_budget("A"), 65);
    let typed_shape = GraphShape::from_workflow(&typed(retrying(&["A"], 65), &["A"])).unwrap();
    assert_eq!(typed_shape.attempt_cap(64), 64);
    assert_eq!(typed_shape.retry_budget("A"), 2);

    // A typed Retry node runs at most three attempts.
    let harness = Harness::new();
    let execution = harness.start(typed(retrying(&["A"], 65), &["A"])).await;
    assert_eq!(
        run_with_failures(&harness, execution, None).await,
        Step::Done
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Failed
    );
    let attempts = harness.attempts(execution).await;
    assert_eq!(attempts.len(), 3);
    assert!(
        attempts
            .iter()
            .all(|a| a.failure_class.as_deref() == Some(rows::failure::SESSION_FAILED))
    );

    // Without `Retry`, a typed node has exactly one attempt.
    let once = harness.start(typed(workflow(&["A"], &[]), &["A"])).await;
    assert_eq!(run_with_failures(&harness, once, None).await, Step::Done);
    assert_eq!(harness.attempts(once).await.len(), 1);

    // The execution cap binds: 22 typed Retry nodes would need 66 attempts,
    // and the typed execution stops within 64. The same legacy shape runs
    // all 66.
    let nodes: Vec<String> = (0..22).map(|n| format!("N{n}")).collect();
    let nodes: Vec<&str> = nodes.iter().map(String::as_str).collect();
    let capped = harness.start(typed(retrying(&nodes, 2), &nodes)).await;
    assert_eq!(run_with_failures(&harness, capped, None).await, Step::Done);
    assert_eq!(harness.status(capped).await, rows::ExecutionStatus::Failed);
    assert!(harness.attempts(capped).await.len() <= 64);
    let error = rows::load_execution(&*harness.executor.store.lock().await, capped)
        .unwrap()
        .unwrap()
        .error
        .unwrap();
    assert!(error.contains("max_node_attempts"));
    let legacy = harness.start(retrying(&nodes, 2)).await;
    assert_eq!(run_with_failures(&harness, legacy, None).await, Step::Done);
    assert_eq!(harness.attempts(legacy).await.len(), 66);
}

// ─── #1641 S1a: review nodes ────────────────────────────────────────────────

fn review_step(of: &str, max_rounds: u8) -> serde_json::Value {
    serde_json::json!({
        "kind": "review",
        "of": of,
        "reviewer": {"provider": "codex", "model": "gpt-6-luna", "effort": "medium"},
        "max_rounds": max_rounds,
    })
}

const REVIEW_ROUTING: [(&str, &str, &str); 2] = [
    ("Review", "Fix", "verdict_changes_requested"),
    ("Review", "Land", "verdict_accepted"),
];

/// implement → review; review ⇒ fix (changes) → review (loop); review ⇒ land.
fn review_loop(max_rounds: u8, until: u32) -> WorkflowDefinition {
    let looped = with_loops(
        workflow(
            &["Impl", "Review", "Fix", "Land"],
            &[("Impl", "Review"), ("Review", "Fix"), ("Review", "Land")],
        ),
        &[("Fix", "Review")],
        &[&["Fix", "Review"]],
        &UntilCondition::MaxIterations(until),
    );
    with_steps(
        looped,
        &[
            (
                "Impl",
                serde_json::json!({"kind": "session", "expects_commit": true}),
            ),
            ("Review", review_step("Impl", max_rounds)),
            (
                "Fix",
                serde_json::json!({"kind": "session", "expects_commit": true, "pass_content": true}),
            ),
        ],
        &REVIEW_ROUTING,
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn review_node_waits_without_a_session_then_accepted_routes_to_land_and_skips_fix() {
    let harness = Harness::new();
    let execution = harness.start(review_loop(2, 3)).await;
    // Implement runs and completes; the review has no verdict yet.
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let implement = harness.attempt(execution, "Impl", 0, 1).await;
    let commit = harness.commit_and_complete(implement.session_id, "impl");
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    let review = harness.attempt(execution, "Review", 0, 1).await;
    assert_eq!(review.status, AttemptStatus::Waiting);
    assert_eq!(review.node_kind, "review");
    assert_eq!(review.base_commit, commit);
    let requests = harness.review_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(review.review_assignment_id, Some(requests[0].0));
    assert_eq!(requests[0].1.of_node, "Impl");
    assert_eq!(requests[0].1.round, 1);
    assert_eq!(requests[0].1.source_commit, commit);
    // Waiting is stable across ticks and launches nothing of its own.
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    assert_eq!(harness.launches().len(), 1);
    assert_eq!(harness.review_requests().len(), 1);

    harness.script_reviews([ReviewScript::Accept]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let review = harness.attempt(execution, "Review", 0, 1).await;
    assert_eq!(review.status, AttemptStatus::Succeeded);
    assert_eq!(review.result_commit.as_deref(), Some(commit.as_str()));
    let output = review.output.unwrap();
    assert_eq!(output["fields"]["verdict"], "accepted");
    assert_eq!(output["fields"]["round"], 1.0);
    assert_eq!(
        harness.attempt(execution, "Fix", 0, 1).await.status,
        AttemptStatus::Skipped
    );
    let land = harness.attempt(execution, "Land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert_eq!(land.base_commit, commit);
    // Only the implement and land sessions ever launched.
    assert_eq!(harness.launches().len(), 2);
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn review_changes_requested_passes_findings_to_fix_and_loops() {
    let harness = Harness::new();
    let execution = harness.start(review_loop(3, 3)).await;
    harness.script_reviews([
        ReviewScript::Changes(vec!["handle the empty case", "name the constant"]),
        ReviewScript::Accept,
    ]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);

    let first = harness.attempt(execution, "Review", 0, 1).await;
    assert_eq!(first.status, AttemptStatus::Succeeded);
    assert_eq!(
        first.output.as_ref().unwrap()["fields"]["verdict"],
        "changes_requested"
    );
    let fix = harness.attempt(execution, "Fix", 0, 1).await;
    assert_eq!(fix.status, AttemptStatus::Succeeded);
    assert!(fix.query().contains("1. handle the empty case"));
    assert!(fix.query().contains("2. name the constant"));
    // The fix forks from the commit that was reviewed.
    assert_eq!(
        Some(fix.base_commit.as_str()),
        first.result_commit.as_deref()
    );

    // The second round reviews the fix's commit and accepts it.
    let second = harness.attempt(execution, "Review", 1, 1).await;
    assert_eq!(second.base_commit, fix.result_commit.clone().unwrap());
    assert_eq!(second.output.as_ref().unwrap()["fields"]["round"], 2.0);
    assert_eq!(
        second.output.as_ref().unwrap()["fields"]["verdict"],
        "accepted"
    );
    assert_eq!(
        harness.attempt(execution, "Fix", 1, 1).await.status,
        AttemptStatus::Skipped
    );
    // The accepted review ends the loop: no third round is reserved.
    assert!(
        harness
            .attempts(execution)
            .await
            .iter()
            .all(|attempt| attempt.iteration < 2)
    );
    let land = harness.attempt(execution, "Land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Succeeded);
    assert_eq!(land.base_commit, second.result_commit.unwrap());
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn review_rounds_exhausted_blocks_with_typed_reason() {
    let harness = Harness::new();
    let execution = harness.start(review_loop(2, 5)).await;
    harness.script_reviews([
        ReviewScript::Changes(vec!["first"]),
        ReviewScript::Changes(vec!["still wrong"]),
    ]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let last = harness.attempt(execution, "Review", 1, 1).await;
    assert_eq!(last.status, AttemptStatus::Blocked);
    assert_eq!(
        last.failure_class.as_deref(),
        Some(rows::failure::REVIEW_ROUNDS_EXHAUSTED)
    );
    assert!(last.error.unwrap().contains("still wrong"));
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Blocked
    );
    let reason = {
        let store = harness.executor.store.lock().await;
        rows::load_execution(&store, execution)
            .unwrap()
            .unwrap()
            .blocked_reason
            .unwrap()
    };
    assert_eq!(reason["kind"], "review_rounds_exhausted");
    assert_eq!(reason["node_id"], "Review");
    // Nothing landed.
    assert!(
        harness
            .attempts(execution)
            .await
            .iter()
            .all(|attempt| attempt.node_id != "Land")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn review_rounds_stop_at_the_loop_bound_even_below_max_rounds() {
    // Three rounds are allowed but the loop itself stops after two.
    let harness = Harness::new();
    let execution = harness.start(review_loop(3, 2)).await;
    harness.script_reviews([
        ReviewScript::Changes(vec!["a"]),
        ReviewScript::Changes(vec!["b"]),
    ]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let last = harness.attempt(execution, "Review", 1, 1).await;
    assert_eq!(last.status, AttemptStatus::Blocked);
    assert_eq!(
        last.failure_class.as_deref(),
        Some(rows::failure::REVIEW_ROUNDS_EXHAUSTED)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn review_unsettled_retries_once_then_blocks() {
    // No usable verdict twice: one new assignment, then blocked.
    let harness = Harness::new();
    let execution = harness.start(review_loop(2, 3)).await;
    harness.script_reviews([
        ReviewScript::Unsettled("superseded"),
        ReviewScript::Unsettled("superseded again"),
    ]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let first = harness.attempt(execution, "Review", 0, 1).await;
    let second = harness.attempt(execution, "Review", 0, 2).await;
    assert_eq!(first.status, AttemptStatus::Failed);
    assert_eq!(second.status, AttemptStatus::Blocked);
    for attempt in [&first, &second] {
        assert_eq!(
            attempt.failure_class.as_deref(),
            Some(rows::failure::REVIEW_UNSETTLED)
        );
    }
    assert_ne!(first.review_assignment_id, second.review_assignment_id);
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Blocked
    );

    // A usable verdict on the replacement assignment completes the review.
    let harness = Harness::new();
    let execution = harness.start(review_loop(2, 3)).await;
    harness.script_reviews([ReviewScript::Unsettled("superseded"), ReviewScript::Accept]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    assert_eq!(
        harness.attempt(execution, "Review", 0, 2).await.status,
        AttemptStatus::Succeeded
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn refused_review_request_is_unsettled_and_retried_once() {
    let harness = Harness::new();
    harness
        .executor
        .effects
        .review_request_failures
        .store(1, Ordering::SeqCst);
    let execution = harness.start(review_loop(2, 3)).await;
    harness.script_reviews([ReviewScript::Accept]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let refused = harness.attempt(execution, "Review", 0, 1).await;
    assert_eq!(refused.status, AttemptStatus::Failed);
    assert_eq!(
        refused.failure_class.as_deref(),
        Some(rows::failure::REVIEW_UNSETTLED)
    );
    assert!(refused.error.unwrap().contains("review service refused"));
    assert_eq!(
        harness.attempt(execution, "Review", 0, 2).await.status,
        AttemptStatus::Succeeded
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn waiting_review_survives_a_restart_and_re_derives_its_route() {
    let mut harness = Harness::new();
    let execution = harness.start(review_loop(2, 3)).await;
    harness.executor.advance(execution).await.unwrap();
    let implement = harness.attempt(execution, "Impl", 0, 1).await;
    harness.commit_and_complete(implement.session_id, "impl");
    harness.executor.advance(execution).await.unwrap();
    assert_eq!(
        harness.attempt(execution, "Review", 0, 1).await.status,
        AttemptStatus::Waiting
    );
    harness.restart();
    // The restarted incarnation still drives the waiting execution and does
    // not open a second assignment.
    let ids = {
        let store = harness.executor.store.lock().await;
        rows::drivable_execution_ids(&store, 10).unwrap()
    };
    assert_eq!(ids, vec![execution]);
    harness.script_reviews([ReviewScript::Accept]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    assert_eq!(harness.review_requests().len(), 1);
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn cancelling_abandons_a_waiting_review() {
    let harness = Harness::new();
    let execution = harness.start(review_loop(2, 3)).await;
    harness.executor.advance(execution).await.unwrap();
    let implement = harness.attempt(execution, "Impl", 0, 1).await;
    harness.commit_and_complete(implement.session_id, "impl");
    harness.executor.advance(execution).await.unwrap();
    harness.executor.request_interrupt(execution).await.unwrap();
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Done
    );
    assert_eq!(
        harness.attempt(execution, "Review", 0, 1).await.status,
        AttemptStatus::Cancelled
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Cancelled
    );
}

// ─── review validation ──────────────────────────────────────────────────────

fn refusal(workflow: WorkflowDefinition) -> String {
    crate::topology::steps::validate_workflow(&workflow).expect_err("definition is refused")
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn verdict_edge_from_non_review_node_is_refused() {
    let session = stamp_steps(
        workflow(&["A", "B"], &[("A", "B")]),
        &[("A", serde_json::json!({"kind": "session"}))],
        &[("A", "B", "verdict_accepted")],
    );
    assert!(refusal(session).contains("verdict edges must leave a review node"));
    // Legacy untyped nodes cannot route a verdict either.
    let legacy = stamp_steps(
        workflow(&["A", "B"], &[("A", "B")]),
        &[],
        &[("A", "B", "verdict_changes_requested")],
    );
    assert!(refusal(legacy).contains("verdict edges must leave a review node"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn review_node_validation_rules() {
    let committer = serde_json::json!({"kind": "session", "expects_commit": true});
    let build = |of: serde_json::Value, rounds: u8, when: &str| {
        let mut step = review_step("A", rounds);
        step["of"] = of;
        stamp_steps(
            workflow(&["A", "R", "L"], &[("A", "R"), ("R", "L")]),
            &[("A", committer.clone()), ("R", step)],
            &[("R", "L", when)],
        )
    };
    // The accepted shape validates.
    crate::topology::steps::validate_workflow(&build("A".into(), 3, "verdict_accepted")).unwrap();
    assert!(
        refusal(build("A".into(), 4, "verdict_accepted"))
            .contains("max_rounds must be between 1 and 3")
    );
    assert!(
        refusal(build("A".into(), 0, "verdict_accepted"))
            .contains("max_rounds must be between 1 and 3")
    );
    assert!(refusal(build("Z".into(), 2, "verdict_accepted")).contains("names unknown node Z"));
    // `of` must be a commit-producing session that runs before the review.
    assert!(
        refusal(build("L".into(), 2, "verdict_accepted"))
            .contains("must be a session node with expects_commit")
    );
    // An unconditioned edge would carry a rejected commit onward.
    assert!(
        refusal(build("A".into(), 2, "success"))
            .contains("must be verdict_accepted or verdict_changes_requested")
    );
    // A session without expects_commit has no commit to review.
    let plain = stamp_steps(
        workflow(&["A", "R"], &[("A", "R")]),
        &[
            ("A", serde_json::json!({"kind": "session"})),
            ("R", review_step("A", 2)),
        ],
        &[],
    );
    assert!(refusal(plain).contains("must be a session node with expects_commit"));
    // Unknown reviewer fields are hard errors, like every other step field.
    let strict: std::result::Result<rsi_common::types::TopologyStep, _> =
        serde_json::from_value(serde_json::json!({
            "kind": "review", "of": "A",
            "reviewer": {"provider": "codex", "model": "m", "effort": "high", "argv": []},
        }));
    assert!(strict.is_err());
    // `max_rounds` defaults to 2.
    let defaulted: rsi_common::types::TopologyStep = serde_json::from_value(serde_json::json!({
        "kind": "review", "of": "A",
        "reviewer": {"provider": "codex", "model": "m", "effort": "high"},
    }))
    .unwrap();
    assert!(matches!(
        defaulted,
        rsi_common::types::TopologyStep::Review { max_rounds: 2, .. }
    ));
}

// ─── #1641 S1b: what the review service is told ─────────────────────────────

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn review_request_names_the_author_pin_and_previous_round() {
    let harness = Harness::new();
    let execution = harness.start(review_loop(3, 3)).await;
    harness.script_reviews([
        ReviewScript::Changes(vec!["handle the empty case"]),
        ReviewScript::Accept,
    ]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);

    let implement = harness.attempt(execution, "Impl", 0, 1).await;
    let fix = harness.attempt(execution, "Fix", 0, 1).await;
    let requests = harness.review_requests();
    assert_eq!(requests.len(), 2);

    // Round one reviews the author's own pinned commit.
    let (first_id, first) = &requests[0];
    assert_eq!(first.round, 1);
    assert_eq!(first.author_session_id, implement.session_id);
    assert!(first.extra_contributors.is_empty());
    assert_eq!(first.previous_assignment, None);
    assert_eq!(
        first.pin_ref,
        crate::topology::custody::node_pin_ref(execution, "Impl", 0)
    );
    assert_eq!(
        first.source_commit,
        implement.result_commit.clone().unwrap()
    );
    assert!(
        first.query.contains("produced by node `Impl`"),
        "{}",
        first.query
    );
    assert!(
        first.query.contains(&first.source_commit),
        "{}",
        first.query
    );

    // Round two reviews the fix node's commit: the author stays the first
    // reviewed commit's author, the fix session is a further contributor, and
    // the previous round's assignment is the delta to resolve.
    let (_, second) = &requests[1];
    assert_eq!(second.round, 2);
    assert_eq!(second.author_session_id, implement.session_id);
    assert_eq!(second.extra_contributors, vec![fix.session_id]);
    assert_eq!(second.previous_assignment, Some(*first_id));
    assert_eq!(
        second.pin_ref,
        crate::topology::custody::node_pin_ref(execution, "Fix", 0)
    );
    assert_eq!(second.source_commit, fix.result_commit.clone().unwrap());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn review_without_a_manager_ledger_blocks_at_once_with_its_typed_class() {
    let harness = Harness::new();
    *harness.executor.effects.review_refusal.lock().unwrap() =
        Some("review_no_manager_ledger: the project has no live manager".into());
    let execution = harness.start(review_loop(2, 3)).await;
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let review = harness.attempt(execution, "Review", 0, 1).await;
    assert_eq!(review.status, AttemptStatus::Blocked);
    assert_eq!(
        review.failure_class.as_deref(),
        Some(rows::failure::REVIEW_NO_MANAGER_LEDGER)
    );
    assert!(review.error.unwrap().contains("no live manager"));
    // No second assignment is tried: retrying the same request cannot help.
    assert!(
        harness
            .attempts(execution)
            .await
            .iter()
            .all(|attempt| attempt.node_id != "Review" || attempt.attempt_no == 1)
    );
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Blocked
    );
}

// ─── #1641 S2: land nodes ───────────────────────────────────────────────────

/// implement → review ⇒ land (a real land step, not a session).
fn land_flow() -> WorkflowDefinition {
    with_steps(
        workflow(
            &["Impl", "Review", "Land"],
            &[("Impl", "Review"), ("Review", "Land")],
        ),
        &[
            (
                "Impl",
                serde_json::json!({"kind": "session", "expects_commit": true}),
            ),
            ("Review", review_step("Impl", 2)),
            (
                "Land",
                serde_json::json!({"kind": "land", "accepted": "Review", "test_filters": ["rsid=topology"]}),
            ),
        ],
        &[("Review", "Land", "verdict_accepted")],
    )
}

/// Run `land_flow` until the land attempt is waiting on the queue.
async fn land_waiting(harness: &Harness) -> (Uuid, AttemptRow) {
    let execution = harness.start(land_flow()).await;
    harness.script_reviews([ReviewScript::Accept]);
    for _ in 0..8 {
        harness.executor.advance(execution).await.unwrap();
        for attempt in harness.attempts(execution).await {
            if attempt.status == AttemptStatus::Running {
                harness.commit_and_complete(attempt.session_id, "impl");
            }
        }
        let land = harness.attempts(execution).await;
        if let Some(land) = land.iter().find(|a| a.node_id == "Land")
            && land.status == AttemptStatus::Waiting
        {
            return (execution, land.clone());
        }
    }
    panic!("the land node never reached the queue");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_enqueues_once_and_mirrors_published() {
    let harness = Harness::new();
    let (execution, land) = land_waiting(&harness).await;
    let accepted = harness.attempt(execution, "Review", 0, 1).await;
    assert_eq!(land.node_kind, "land");
    assert_eq!(land.base_commit, accepted.result_commit.clone().unwrap());
    let requests = harness.land_requests();
    assert_eq!(requests.len(), 1);
    let (entry, request) = &requests[0];
    assert_eq!(land.land_entry_id, Some(*entry));
    // The queue is asked for the accepted commit, the accepting assignment,
    // the reviewed author, and the attempt's own replay key.
    assert_eq!(request.source_commit, land.base_commit);
    assert_eq!(
        request.review_assignment_id,
        accepted.review_assignment_id.unwrap()
    );
    assert_eq!(request.review_node, "Review");
    assert_eq!(
        request.author_session_id,
        harness.attempt(execution, "Impl", 0, 1).await.session_id
    );
    assert_eq!(request.dedup_key, land.dedup_key);
    assert_eq!(request.test_filters, vec!["rsid=topology".to_owned()]);
    assert_eq!(request.repo_root, harness.repo);

    // Waiting is stable across ticks, with no second enqueue and no launch.
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    assert_eq!(harness.land_requests().len(), 1);
    assert_eq!(harness.launches().len(), 1);

    // A gating entry is still pending; the published entry settles the node.
    harness.set_land_state(*entry, LandStatus::Pending);
    assert_eq!(
        harness.executor.advance(execution).await.unwrap(),
        Step::Wait
    );
    harness.set_land_state(
        *entry,
        LandStatus::Published {
            landed_sha: Some("f".repeat(40)),
        },
    );
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let land = harness.attempt(execution, "Land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Succeeded);
    let output = land.output.unwrap();
    assert_eq!(output["fields"]["state"], "published");
    assert_eq!(output["fields"]["landed_sha"], "f".repeat(40));
    assert_eq!(output["fields"]["entry_id"], entry.to_string());
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Succeeded
    );
    assert_eq!(harness.land_requests().len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_after_restart_mirrors_existing_entry_without_resubmitting() {
    let mut harness = Harness::new();
    let (execution, land) = land_waiting(&harness).await;
    let entry = land.land_entry_id.unwrap();
    harness.restart();
    let ids = {
        let store = harness.executor.store.lock().await;
        rows::drivable_execution_ids(&store, 10).unwrap()
    };
    assert_eq!(ids, vec![execution]);
    harness.set_land_state(
        entry,
        LandStatus::Published {
            landed_sha: Some("e".repeat(40)),
        },
    );
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    assert_eq!(harness.land_requests().len(), 1);
    assert_eq!(
        harness.attempt(execution, "Land", 0, 1).await.status,
        AttemptStatus::Succeeded
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_adopts_an_entry_enqueued_before_a_crash_without_a_second_enqueue() {
    let mut harness = Harness::new();
    let execution = harness.start(land_flow()).await;
    harness.script_reviews([ReviewScript::Accept]);
    // Drive until the land attempt is reserved, then simulate the crash
    // window: the queue holds the entry, the attempt row does not know it.
    for _ in 0..8 {
        harness.executor.advance(execution).await.unwrap();
        for attempt in harness.attempts(execution).await {
            if attempt.status == AttemptStatus::Running {
                harness.commit_and_complete(attempt.session_id, "impl");
            }
        }
        if harness.land_requests().len() == 1 {
            break;
        }
    }
    let (entry, _) = harness.land_requests()[0].clone();
    {
        let store = harness.executor.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE topology_node_attempts SET status='reserved',integrate_action_id=NULL \
                 WHERE execution_id=?1 AND node_id='Land'",
                [execution.to_string()],
            )
            .unwrap();
    }
    harness.restart();
    harness.executor.advance(execution).await.unwrap();
    let land = harness.attempt(execution, "Land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Waiting);
    assert_eq!(land.land_entry_id, Some(entry));
    assert_eq!(harness.land_requests().len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_refused_entry_fails_without_retry() {
    for state in ["refused", "failed", "superseded"] {
        let harness = Harness::new();
        let (execution, land) = land_waiting(&harness).await;
        harness.set_land_state(
            land.land_entry_id.unwrap(),
            LandStatus::Refused {
                state: state.into(),
                reason: "queue_batch_merge_conflict".into(),
            },
        );
        assert_eq!(harness.run_to_end(execution).await, Step::Done);
        let land = harness.attempt(execution, "Land", 0, 1).await;
        assert_eq!(land.status, AttemptStatus::Failed, "{state}");
        assert_eq!(
            land.failure_class.as_deref(),
            Some(rows::failure::LAND_REFUSED)
        );
        assert!(land.error.unwrap().contains("queue_batch_merge_conflict"));
        // Never retried: one attempt, one enqueue, a failed execution.
        assert!(
            harness
                .attempts(execution)
                .await
                .iter()
                .all(|a| a.node_id != "Land" || a.attempt_no == 1)
        );
        assert_eq!(harness.land_requests().len(), 1);
        assert_eq!(
            harness.status(execution).await,
            rows::ExecutionStatus::Failed
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_refuses_when_acceptance_changed() {
    let harness = Harness::new();
    harness.world.lock().unwrap().land_override = Some(LandEnqueue::AdmissionLost(
        "the review no longer admits this commit as accepted".into(),
    ));
    let execution = harness.start(land_flow()).await;
    harness.script_reviews([ReviewScript::Accept]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let land = harness.attempt(execution, "Land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Blocked);
    assert_eq!(
        land.failure_class.as_deref(),
        Some(rows::failure::LAND_ADMISSION_LOST)
    );
    assert!(land.error.unwrap().contains("no longer admits"));
    assert_eq!(land.land_entry_id, None);
    assert!(harness.land_requests().is_empty());
    assert_eq!(
        harness.status(execution).await,
        rows::ExecutionStatus::Blocked
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_cancelled_before_the_entry_exists_cancels_the_attempt() {
    let harness = Harness::new();
    harness.world.lock().unwrap().land_override = Some(LandEnqueue::Cancelled);
    let execution = harness.start(land_flow()).await;
    harness.script_reviews([ReviewScript::Accept]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let land = harness.attempt(execution, "Land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Cancelled);
    assert_eq!(
        land.failure_class.as_deref(),
        Some(rows::failure::CANCELLED)
    );
    assert_eq!(land.land_entry_id, None);
    assert!(harness.land_requests().is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn land_with_a_disabled_queue_blocks_visibly_and_a_queue_refusal_fails() {
    let harness = Harness::new();
    harness.world.lock().unwrap().land_override = Some(LandEnqueue::QueueDisabled);
    let execution = harness.start(land_flow()).await;
    harness.script_reviews([ReviewScript::Accept]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let land = harness.attempt(execution, "Land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Blocked);
    assert_eq!(
        land.failure_class.as_deref(),
        Some(rows::failure::QUEUE_DISABLED)
    );
    assert!(harness.land_requests().is_empty());

    let harness = Harness::new();
    harness.world.lock().unwrap().land_override =
        Some(LandEnqueue::Refused("queue_duplicate_source".into()));
    let execution = harness.start(land_flow()).await;
    harness.script_reviews([ReviewScript::Accept]);
    assert_eq!(harness.run_to_end(execution).await, Step::Done);
    let land = harness.attempt(execution, "Land", 0, 1).await;
    assert_eq!(land.status, AttemptStatus::Failed);
    assert_eq!(
        land.failure_class.as_deref(),
        Some(rows::failure::LAND_REFUSED)
    );
    assert!(land.error.unwrap().contains("queue_duplicate_source"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn land_node_validation_rules() {
    let committer = serde_json::json!({"kind": "session", "expects_commit": true});
    let land = |accepted: &str, filters: serde_json::Value| serde_json::json!({"kind": "land", "accepted": accepted, "test_filters": filters});
    let build = |step: serde_json::Value, when: &str| {
        stamp_steps(
            workflow(&["A", "R", "L"], &[("A", "R"), ("R", "L")]),
            &[
                ("A", committer.clone()),
                ("R", review_step("A", 2)),
                ("L", step),
            ],
            &[("R", "L", when)],
        )
    };
    crate::topology::steps::validate_workflow(&build(
        land("R", serde_json::json!([])),
        "verdict_accepted",
    ))
    .unwrap();
    // `accepted` must name a review node.
    assert!(
        refusal(build(land("A", serde_json::json!([])), "verdict_accepted"))
            .contains("must be a review node")
    );
    assert!(
        refusal(build(land("Z", serde_json::json!([])), "verdict_accepted"))
            .contains("names unknown node Z")
    );
    // The only input is the review's verdict_accepted edge.
    assert!(
        refusal(build(
            land("R", serde_json::json!([])),
            "verdict_changes_requested"
        ))
        .contains("only input must be the verdict_accepted edge")
    );
    let two_inputs = stamp_steps(
        workflow(&["A", "R", "L"], &[("A", "R"), ("R", "L"), ("A", "L")]),
        &[
            ("A", committer.clone()),
            ("R", review_step("A", 2)),
            ("L", land("R", serde_json::json!([]))),
        ],
        &[("R", "L", "verdict_accepted")],
    );
    assert!(refusal(two_inputs).contains("only input must be the verdict_accepted edge"));
    // Queue filters are validated with the queue's own PACKAGE=FILTER rule.
    for bad in ["nofilter", "=x", "rsid=", "-p=x"] {
        assert!(
            refusal(build(
                land("R", serde_json::json!([bad])),
                "verdict_accepted"
            ))
            .contains("PACKAGE=FILTER"),
            "{bad}"
        );
    }
    // The step decodes strictly.
    let strict: std::result::Result<rsi_common::types::TopologyStep, _> =
        serde_json::from_value(serde_json::json!({"kind": "land", "accepted": "R", "argv": []}));
    assert!(strict.is_err());
}
