//! Provider-free acceptance tests for the durable topology executor (#634,
//! plan §7 T2-A1..A14). A fake effect boundary allocates real Git worktree
//! sandboxes and records real `model_invocations` admissions, so custody,
//! pins, preservation and the dedup fence run for real; only the provider
//! process is simulated. A "restart" drops every handle and reopens the store
//! (a new daemon boot id) while the fake's external world survives.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
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
use crate::topology::executor::{Executor, LaunchRequest, NodeEffects, SessionObservation, Step};
use crate::topology::recovery::recover_after_restart;
use crate::topology::store::{self as rows, AttemptRow, AttemptStatus, NewAttempt, NewExecution};

// ─── fixture ────────────────────────────────────────────────────────────────

fn git(dir: &Path, args: &[&str]) -> String {
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
}

/// One catalog-op run of the current incarnation.
struct FakeRun {
    sandbox: PathBuf,
    outcome: Option<CommandOutcome>,
}

/// Process group the fake reports for every op.
const FAKE_PGID: i32 = 4242;

struct Fake {
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
    record_hold: StdMutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
}

impl NodeEffects for Fake {
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

    fn kill_stale_group(&self, pgid: i32) {
        self.world.lock().unwrap().killed_groups.push(pgid);
    }

    fn build_node_cap(&self) -> u32 {
        self.build_cap.load(Ordering::SeqCst)
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

struct Harness {
    _dirs: (TempDir, TempDir),
    repo: PathBuf,
    base: String,
    db: PathBuf,
    sandboxes: PathBuf,
    world: Arc<StdMutex<World>>,
    executor: Executor<Fake>,
}

impl Harness {
    fn new() -> Self {
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
    fn restart(&mut self) {
        let fresh = boot(&self.db, &self.sandboxes, &self.world);
        let old = std::mem::replace(&mut self.executor, fresh);
        drop(old);
    }

    async fn start(&self, workflow: WorkflowDefinition) -> Uuid {
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
        };
        let store = self.executor.store.lock().await;
        rows::insert_execution(&store, &new).unwrap();
        new.id
    }

    async fn attempts(&self, execution_id: Uuid) -> Vec<AttemptRow> {
        let store = self.executor.store.lock().await;
        rows::load_attempts(&store, execution_id).unwrap()
    }

    async fn attempt(
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

    async fn status(&self, execution_id: Uuid) -> rows::ExecutionStatus {
        let store = self.executor.store.lock().await;
        rows::load_execution(&store, execution_id)
            .unwrap()
            .unwrap()
            .status
    }

    async fn row_version(&self, execution_id: Uuid) -> i64 {
        let store = self.executor.store.lock().await;
        rows::current_row_version(&store, execution_id).unwrap()
    }

    fn launches(&self) -> Vec<(String, Uuid, String)> {
        self.world.lock().unwrap().launches.clone()
    }

    fn sandbox(&self, session_id: Uuid) -> PathBuf {
        self.world.lock().unwrap().sessions[&session_id]
            .sandbox
            .clone()
    }

    fn set_status(&self, session_id: Uuid, status: SessionStatus) {
        self.world
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session_id)
            .unwrap()
            .status = status;
    }

    /// The node's agent commits one change and its session completes.
    fn commit_and_complete(&self, session_id: Uuid, label: &str) -> String {
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

    /// The running catalog op of `attempt_id` exits with `exit_code`.
    fn finish_command(&self, attempt_id: Uuid, exit_code: i32) {
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
    async fn run_to_end(&self, execution_id: Uuid) -> Step {
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
    crate::topology::steps::validate_workflow(&workflow).expect("valid steps");
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

fn error_code(error: &DaemonError) -> String {
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

/// T2-A2: an attempt reserved but never launched relaunches after a restart
/// with exactly the reserved dedup key and pre-minted session id.
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

/// Round 1 (`concurrent_test_does_not_race`): two advancers on separate
/// worker threads race for real. The first is suspended inside the
/// reservation window (ready set decided, transaction not yet run) while the
/// second is released against the same execution; each node still gets
/// exactly one attempt, one admission row and one launch.
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
