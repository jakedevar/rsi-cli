//! Test-only seeder for an ISOLATED rsid state directory (local #122).
//!
//! The desktop end-to-end harness needs a finished leaf session with a conversation in a throwaway
//! daemon, without launching a provider. This tool writes it through the real `Store` API
//! (`insert_session`, `insert_event`) into the isolated `rsi.db` while its daemon is stopped, so every
//! schema trigger and SQL function is the store's own and nothing is stubbed.
//!
//! It cannot reach production:
//! - It is a separate binary behind the `e2e-seed` cargo feature (`required-features`); no default or
//!   release build compiles it, and `crates/rsid/build.rs` refuses the feature in the release profile
//!   (`scripts/check-release-seam.sh` refuses it in the release dependency graph before `make release`).
//! - It is not an RPC, agent verb or daemon flag: the daemon has no code path that imports a fixture, so
//!   neither an agent nor an operator can fabricate history through the socket.
//! - Even when built, it writes only to a state directory that carries the harness's marker file, lies
//!   under the system temp directory, and is not the operator's `~/.rsi` itself; and the database it opens
//!   is a plain file in that directory: an `rsi.db` or `-wal`/`-shm`/`-journal` sibling that is a symlink,
//!   not a regular file, hard-linked elsewhere, outside the directory or the operator's own `rsi.db` is
//!   refused before `Store::open` (hub #3245).
//!
//! Local #150 (hub #3253): the spec may also carry a `graph_run`, a FINISHED, SUCCEEDED durable graph run of
//! a saved workflow: the workflow row (`Store::upsert_workflow_definition`), its `topology_executions` row
//! ending `succeeded`, one `succeeded` session-node attempt per node with its result, and the
//! `topology_events` stream. The topology writers are private to the daemon crate, so the run is written
//! through the opened `Store`'s own connection (every migration, CHECK and trigger applies) in one
//! transaction, with the same rows, event kinds and `GraphExecutionUpdate` payloads the executor appends for
//! a run that reserves, runs and settles each node and then succeeds (`topology/store.rs` `insert_in`,
//! `insert_attempts`, `mark_running`, `settle_in`, `transition_execution`). The daemon reads it back with
//! its own `GetWorkflowExecution` projection; its cleanup pass settles the rest on boot.
//!
//! Usage: `rsi-e2e-seed --state-dir <dir> --spec <spec.json>`; the database is `<dir>/rsi.db`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rsi_common::types::{
    ConversationEvent, GraphExecutionUpdate, Session, WorkflowDocument, WorkflowExecutionStatus,
    WorkflowNodeExecutionState,
};
use rsi_graph::data::NodeData;
use rsi_graph::format::WorkflowDefinition;
use rsid::store::Store;
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Digest;
use uuid::Uuid;

/// The file the harness writes into the state directory it created, so the seeder can tell it from any
/// other directory (the operator's included).
const MARKER_FILE: &str = ".rsi-e2e-isolated-state";
const MARKER_TEXT: &str = "rsi-e2e-isolated-state v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    session: SessionSpec,
    events: Vec<EventSpec>,
    /// Local #150: an optional finished, succeeded graph run.
    #[serde(default)]
    graph_run: Option<GraphRunSpec>,
}

/// A finished, succeeded durable graph run of one saved workflow (local #150).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphRunSpec {
    workflow_id: Uuid,
    title: String,
    project_id: Option<Uuid>,
    execution_id: Uuid,
    /// The repository the run was based on, and the commit (lowercase 40-hex).
    repo_root: PathBuf,
    base_commit: String,
    definition: WorkflowDefinition,
    /// Each node's result text, recorded as its settled output; every key must be a node of the definition.
    #[serde(default)]
    node_results: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionSpec {
    id: Uuid,
    query: String,
    title: Option<String>,
    working_dir: PathBuf,
    project_id: Option<Uuid>,
    parent_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventSpec {
    event_type: String,
    role: Option<String>,
    #[serde(default)]
    content: String,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_use_id: Option<String>,
}

/// Refuse any directory that is not a harness-created isolated state directory.
fn check_state_dir(dir: &Path) -> Result<PathBuf, String> {
    let canonical = dir
        .canonicalize()
        .map_err(|error| format!("state dir {}: {error}", dir.display()))?;
    let temp = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let under_temp = canonical.starts_with(&temp)
        || canonical.starts_with("/tmp")
        || canonical.starts_with("/private/tmp");
    if !under_temp {
        return Err(format!(
            "refused: {} is not under the system temp directory",
            canonical.display()
        ));
    }
    if let Some(operator) = dirs::home_dir().map(|home| home.join(".rsi")) {
        let operator = operator.canonicalize().unwrap_or(operator);
        if canonical == operator {
            return Err("refused: that is the operator's ~/.rsi state".to_string());
        }
    }
    let marker = std::fs::read_to_string(canonical.join(MARKER_FILE)).unwrap_or_default();
    if marker.trim() != MARKER_TEXT {
        return Err(format!(
            "refused: {} has no {MARKER_FILE} marker; only a harness-created isolated state directory is seeded",
            canonical.display()
        ));
    }
    Ok(canonical)
}

/// The database file the seeder opens, and the SQLite sidecars `Store::open` would follow with it.
const DB_FILE: &str = "rsi.db";
const DB_SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Refuse a database in `dir` (the canonical state directory) that is not a plain file of its own: a
/// symlinked or non-regular `rsi.db` or sidecar would make `Store::open` write somewhere else (hub #3245).
fn check_db_files(dir: &Path) -> Result<PathBuf, String> {
    let db = dir.join(DB_FILE);
    let names = std::iter::once(DB_FILE.to_string()).chain(
        DB_SIDECARS
            .iter()
            .map(|suffix| format!("{DB_FILE}{suffix}")),
    );
    for name in names {
        let path = dir.join(&name);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("refused: {}: {error}", path.display())),
        };
        if meta.file_type().is_symlink() {
            return Err(format!("refused: {} is a symlink", path.display()));
        }
        if !meta.is_file() {
            return Err(format!("refused: {} is not a regular file", path.display()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.nlink() > 1 {
                return Err(format!("refused: {} has another hard link", path.display()));
            }
        }
    }
    if db.exists() {
        let canonical = db
            .canonicalize()
            .map_err(|error| format!("refused: {}: {error}", db.display()))?;
        if canonical.parent() != Some(dir) {
            return Err(format!(
                "refused: {} resolves outside the state directory",
                db.display()
            ));
        }
        if let Some(operator) = dirs::home_dir().map(|home| home.join(".rsi").join(DB_FILE))
            && operator
                .canonicalize()
                .is_ok_and(|operator| operator == canonical)
        {
            return Err("refused: that is the operator's ~/.rsi/rsi.db".to_string());
        }
    }
    Ok(db)
}

fn build_session(spec: &SessionSpec, now: &str) -> Result<Session, String> {
    serde_json::from_value(json!({
        "id": spec.id,
        "status": "Completed",
        "session_kind": "Standard",
        "provider": "Claude",
        "created_at": now,
        "updated_at": now,
        "query": spec.query,
        "title": spec.title,
        "working_dir": spec.working_dir,
        "project_id": spec.project_id,
        "parent_id": spec.parent_id,
        "model": "default",
    }))
    .map_err(|error| format!("session spec: {error}"))
}

fn build_event(
    session_id: Uuid,
    sequence: usize,
    spec: &EventSpec,
    now: &str,
) -> Result<ConversationEvent, String> {
    serde_json::from_value(json!({
        "id": 0,
        "session_id": session_id,
        "sequence": sequence,
        "event_type": spec.event_type,
        "role": spec.role,
        "created_at": now,
        "content": spec.content,
        "tool_name": spec.tool_name,
        "tool_input": spec.tool_input,
        "tool_use_id": spec.tool_use_id,
    }))
    .map_err(|error| format!("event {sequence}: {error}"))
}

/// The daemon's namespace for topology attempt and node-session ids (`topology/store.rs`
/// `TOPOLOGY_ID_NAMESPACE`), so a seeded attempt carries the id the executor would have minted.
const TOPOLOGY_ID_NAMESPACE: Uuid = Uuid::from_u128(0x5b0f_6c2e_7d1a_4b39_9e60_2f8d_0c4a_1e77);

fn sha256(text: &str) -> String {
    format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(text.as_bytes()))
    )
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// Refuse a graph run the seeder cannot record as one clean succeeded pass, and return its nodes in run
/// order (every node once, iteration 0, attempt 1).
fn check_graph_run(run: &GraphRunSpec) -> Result<Vec<String>, String> {
    let bad = |why: String| Err(format!("spec: graph_run: {why}"));
    if run.base_commit.len() != 40
        || !run
            .base_commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return bad("base_commit must be a lowercase 40-hex commit".into());
    }
    if run.title.trim().is_empty() {
        return bad("title is empty".into());
    }
    let report = rsi_graph::validate_executable_workflow(&run.definition);
    if report.has_errors() {
        let messages = report
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.is_blocking())
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();
        return bad(format!(
            "the workflow is not executable: {}",
            messages.join("; ")
        ));
    }
    let nodes = run
        .definition
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    if let Some(unknown) = run.node_results.keys().find(|key| !nodes.contains(*key)) {
        return bad(format!("node_results names {unknown}, which is not a node"));
    }
    // Only session nodes: command, gate, review and land nodes have effects a seeded row would fake.
    if let Some(steps) = run.definition.metadata.get("topology_steps") {
        let steps = serde_json::to_value(steps).map_err(|error| error.to_string())?;
        let steps: serde_json::Map<String, Value> = match steps {
            Value::String(text) => serde_json::from_str(&text),
            other => serde_json::from_value(other),
        }
        .map_err(|error| format!("spec: graph_run: topology_steps: {error}"))?;
        if let Some((node, _)) = steps
            .iter()
            .find(|(_, step)| step.get("kind").and_then(Value::as_str) != Some("session"))
        {
            return bad(format!(
                "node {node} is not a session node; only session-node runs are seeded"
            ));
        }
    }
    // Kahn order; a loop would need iterations, which a single clean pass does not have.
    let mut indegree = run
        .definition
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), 0usize))
        .collect::<BTreeMap<_, _>>();
    for edge in &run.definition.edges {
        *indegree.entry(edge.target.as_str()).or_default() += 1;
    }
    let mut ready = run
        .definition
        .nodes
        .iter()
        .filter(|node| indegree[node.id.as_str()] == 0)
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    let mut order = Vec::new();
    while let Some(node) = ready.first().copied() {
        ready.remove(0);
        order.push(node.to_string());
        for edge in run
            .definition
            .edges
            .iter()
            .filter(|edge| edge.source == node)
        {
            let left = indegree
                .get_mut(edge.target.as_str())
                .expect("validated edge");
            *left -= 1;
            if *left == 0 {
                ready.push(edge.target.as_str());
            }
        }
    }
    if order.len() != nodes.len() {
        return bad("the workflow has a loop; only an acyclic run is seeded".into());
    }
    Ok(order)
}

/// The executor's custody plan for an acyclic run: a root forks from the base, any other node from the
/// latest result of its first predecessor (`topology/custody.rs` `NodeForkPlan`).
fn custody_plan(definition: &WorkflowDefinition) -> Value {
    let nodes = definition
        .nodes
        .iter()
        .map(|node| {
            let initial = definition
                .edges
                .iter()
                .filter(|edge| edge.target == node.id)
                .map(|edge| edge.source.clone())
                .min()
                .map_or_else(
                    || json!({ "kind": "base" }),
                    |source| json!({ "kind": "latest", "node": source }),
                );
            (
                node.id.clone(),
                json!({ "initial": initial, "loop_previous": null, "region": null }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({ "nodes": nodes })
}

/// One `topology_events` row, with the `GraphExecutionUpdate` payload the executor's `append_event` writes.
struct Event<'a> {
    kind: &'a str,
    actor: &'a str,
    node_id: Option<&'a str>,
    attempt_id: Option<Uuid>,
    node_state: Option<WorkflowNodeExecutionState>,
    output_preview: Option<String>,
    detail: Value,
}

fn append_event(
    tx: &Connection,
    run: &GraphRunSpec,
    status: WorkflowExecutionStatus,
    event: Event<'_>,
    at: DateTime<Utc>,
) -> Result<(), String> {
    let sequence: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(execution_seq),0)+1 FROM topology_events WHERE execution_id=?1",
            [run.execution_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| format!("event sequence: {error}"))?;
    let update = GraphExecutionUpdate {
        execution_id: run.execution_id,
        workflow_id: run.workflow_id,
        node_id: event.node_id.map(str::to_owned),
        sequence: u64::try_from(sequence).unwrap_or_default(),
        status,
        node_state: event.node_state,
        finished: event.kind == "succeeded",
        error: None,
        output_preview: event.output_preview,
        updated_at: at,
    };
    let payload = json!({ "update": update, "detail": event.detail });
    tx.execute(
        "INSERT INTO topology_events (execution_id,execution_seq,topology_id,node_id,attempt_id,kind,actor_kind,actor_session_id,payload_json,created_at) \
         VALUES (?1,?2,NULL,?3,?4,?5,?6,NULL,?7,?8)",
        params![
            run.execution_id.to_string(),
            sequence,
            event.node_id,
            event.attempt_id.map(|id| id.to_string()),
            event.kind,
            event.actor,
            payload.to_string(),
            rfc3339(at),
        ],
    )
    .map_err(|error| format!("insert event {}: {error}", event.kind))?;
    Ok(())
}

fn bump(tx: &Connection, run: &GraphRunSpec, at: DateTime<Utc>) -> Result<(), String> {
    tx.execute(
        "UPDATE topology_executions SET row_version=row_version+1,updated_at=?2 WHERE id=?1",
        params![run.execution_id.to_string(), rfc3339(at)],
    )
    .map(|_| ())
    .map_err(|error| format!("bump execution: {error}"))
}

/// Write the workflow and its finished, succeeded run (see the module docs).
fn seed_graph_run(store: &Store, run: &GraphRunSpec, order: &[String]) -> Result<(), String> {
    let created = rfc3339(Utc::now());
    let document: WorkflowDocument = serde_json::from_value(json!({
        "workflow": {
            "id": run.workflow_id,
            "title": run.title,
            "stage": "Planning",
            "artifact_path": null,
            "project_id": run.project_id,
            "created_at": created,
            "updated_at": created,
        },
        "definition": run.definition,
    }))
    .map_err(|error| format!("spec: graph_run workflow: {error}"))?;
    let saved = store
        .upsert_workflow_definition(&document)
        .map_err(|error| format!("upsert workflow: {error}"))?;
    let definition: WorkflowDefinition = serde_json::from_value(saved.definition)
        .map_err(|error| format!("stored workflow definition: {error}"))?;
    let definition_json = serde_json::to_string(&definition).map_err(|error| error.to_string())?;

    let tx =
        rusqlite::Transaction::new_unchecked(&store.conn, rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| format!("begin: {error}"))?;
    let exists: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM topology_executions WHERE id=?1)",
            [run.execution_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| format!("execution lookup: {error}"))?;
    if exists {
        return Err(format!(
            "refused: execution {} already exists",
            run.execution_id
        ));
    }
    // A run that took a few seconds and finished a minute ago; one tick per recorded step.
    let start = Utc::now() - chrono::Duration::seconds(60);
    let mut tick = 0i64;
    let mut next = || {
        tick += 1;
        start + chrono::Duration::milliseconds(250 * tick)
    };
    let execution = run.execution_id.to_string();

    let at = next();
    tx.execute(
        "INSERT INTO topology_executions (id,topology_id,topology_revision,topology_name_snapshot,workflow_id,definition_json,definition_digest,project_id,parent_session_id,requested_by_kind,repo_root,base_ref,base_commit,custody_plan_json,status,input_json,max_node_attempts,created_at,updated_at) \
         VALUES (?1,NULL,NULL,?2,?3,?4,?5,?6,NULL,'operator',?7,'refs/remotes/origin/rolling',?8,?9,'accepted',NULL,64,?10,?10)",
        params![
            execution,
            definition.name,
            run.workflow_id.to_string(),
            definition_json,
            sha256(&definition_json),
            run.project_id.map(|id| id.to_string()),
            run.repo_root.display().to_string(),
            run.base_commit,
            custody_plan(&definition).to_string(),
            rfc3339(at),
        ],
    )
    .map_err(|error| format!("insert execution: {error}"))?;
    append_event(
        &tx,
        run,
        WorkflowExecutionStatus::Accepted,
        Event {
            kind: "accepted",
            actor: "operator",
            node_id: None,
            attempt_id: None,
            node_state: None,
            output_preview: None,
            detail: Value::Null,
        },
        at,
    )?;

    let at = next();
    tx.execute(
        "UPDATE topology_executions SET status='running',started_at=?2,row_version=row_version+1,updated_at=?2 WHERE id=?1",
        params![execution, rfc3339(at)],
    )
    .map_err(|error| format!("start execution: {error}"))?;
    append_event(
        &tx,
        run,
        WorkflowExecutionStatus::Running,
        Event {
            kind: "running",
            actor: "executor",
            node_id: None,
            attempt_id: None,
            node_state: None,
            output_preview: None,
            detail: json!({ "from": "accepted", "to": "running", "blocked_reason": null }),
        },
        at,
    )?;

    for node_id in order {
        let node = definition
            .nodes
            .iter()
            .find(|node| &node.id == node_id)
            .expect("ordered node");
        let attempt_id = Uuid::new_v5(
            &TOPOLOGY_ID_NAMESPACE,
            format!("attempt:{execution}:{node_id}:0:1").as_bytes(),
        );
        let session_id = Uuid::new_v5(
            &TOPOLOGY_ID_NAMESPACE,
            format!("session:{attempt_id}").as_bytes(),
        );
        let at = next();
        tx.execute(
            "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,attempt_no,node_kind,status,dedup_key,session_id,base_commit,input_json,created_at,updated_at) \
             VALUES (?1,?2,?3,0,1,'session','reserved',?4,?5,?6,?7,?8,?8)",
            params![
                attempt_id.to_string(),
                execution,
                node_id,
                format!("topology.node:{execution}:{node_id}:0:1"),
                session_id.to_string(),
                run.base_commit,
                json!({ "query": node.instructions }).to_string(),
                rfc3339(at),
            ],
        )
        .map_err(|error| format!("insert attempt {node_id}: {error}"))?;
        append_event(
            &tx,
            run,
            WorkflowExecutionStatus::Running,
            Event {
                kind: "node_reserved",
                actor: "executor",
                node_id: Some(node_id),
                attempt_id: Some(attempt_id),
                node_state: Some(WorkflowNodeExecutionState::Running),
                output_preview: None,
                detail: json!({ "iteration": 0, "attempt_no": 1, "base_commit": run.base_commit }),
            },
            at,
        )?;

        let at = next();
        tx.execute(
            "UPDATE topology_node_attempts SET status='running',started_at=?2,updated_at=?2 WHERE id=?1",
            params![attempt_id.to_string(), rfc3339(at)],
        )
        .map_err(|error| format!("run attempt {node_id}: {error}"))?;
        bump(&tx, run, at)?;
        append_event(
            &tx,
            run,
            WorkflowExecutionStatus::Running,
            Event {
                kind: "node_running",
                actor: "executor",
                node_id: Some(node_id),
                attempt_id: Some(attempt_id),
                node_state: Some(WorkflowNodeExecutionState::Running),
                output_preview: None,
                detail: json!({ "iteration": 0, "attempt_no": 1, "session_id": session_id }),
            },
            at,
        )?;

        // The node's result, recorded as its settled output (`NodeData`) and previewed like the
        // executor's `preview_node_data` (its JSON text, 120 characters at most).
        let output = run.node_results.get(node_id).map(|text| {
            let mut data = NodeData::new();
            data.insert("result", rsi_graph::Value::String(text.clone()));
            data
        });
        let output_json = output
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| error.to_string())?;
        let preview = output_json.as_ref().map(|json| {
            if json.chars().count() > 120 {
                format!("{}...", json.chars().take(120).collect::<String>())
            } else {
                json.clone()
            }
        });
        let at = next();
        tx.execute(
            "UPDATE topology_node_attempts SET status='succeeded',output_json=?2,output_digest=?3,finished_at=?4,updated_at=?4 WHERE id=?1",
            params![
                attempt_id.to_string(),
                output_json,
                output_json.as_deref().map(sha256),
                rfc3339(at),
            ],
        )
        .map_err(|error| format!("settle attempt {node_id}: {error}"))?;
        bump(&tx, run, at)?;
        append_event(
            &tx,
            run,
            WorkflowExecutionStatus::Running,
            Event {
                kind: "node_settled",
                actor: "executor",
                node_id: Some(node_id),
                attempt_id: Some(attempt_id),
                node_state: Some(WorkflowNodeExecutionState::Succeeded),
                output_preview: preview,
                detail: json!({ "iteration": 0, "attempt_no": 1, "status": "succeeded", "failure_class": null, "result_commit": null, "preserved_commit": null }),
            },
            at,
        )?;
    }

    let at = next();
    tx.execute(
        "UPDATE topology_executions SET status='succeeded',finished_at=?2,row_version=row_version+1,updated_at=?2 WHERE id=?1",
        params![execution, rfc3339(at)],
    )
    .map_err(|error| format!("finish execution: {error}"))?;
    append_event(
        &tx,
        run,
        WorkflowExecutionStatus::Succeeded,
        Event {
            kind: "succeeded",
            actor: "executor",
            node_id: None,
            attempt_id: None,
            node_state: None,
            output_preview: None,
            detail: json!({ "from": "running", "to": "succeeded", "blocked_reason": null }),
        },
        at,
    )?;
    tx.commit()
        .map_err(|error| format!("commit graph run: {error}"))
}

fn run(args: &[String]) -> Result<String, String> {
    let (mut state_dir, mut spec_path) = (None, None);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(value)),
            "--spec" => spec_path = Some(PathBuf::from(value)),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let state_dir = state_dir.ok_or("--state-dir is required")?;
    let spec_path = spec_path.ok_or("--spec is required")?;
    let dir = check_state_dir(&state_dir)?;
    let spec: Spec = serde_json::from_str(
        &std::fs::read_to_string(&spec_path)
            .map_err(|error| format!("{}: {error}", spec_path.display()))?,
    )
    .map_err(|error| format!("spec: {error}"))?;
    if spec.events.is_empty() {
        return Err("spec: a finished session needs at least one event".to_string());
    }
    // Every refusal comes before the database is opened, so a bad spec writes nothing.
    let graph_order = spec.graph_run.as_ref().map(check_graph_run).transpose()?;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let session = build_session(&spec.session, &now)?;
    let db = check_db_files(&dir)?;
    let store = Store::open(&db).map_err(|error| format!("open store: {error}"))?;
    store
        .insert_session(&session)
        .map_err(|error| format!("insert session: {error}"))?;
    for (index, event) in spec.events.iter().enumerate() {
        let event = build_event(session.id, index + 1, event, &now)?;
        store
            .insert_event(&event)
            .map_err(|error| format!("insert event {}: {error}", index + 1))?;
    }
    let mut summary = format!(
        "seeded session {} with {} events",
        session.id,
        spec.events.len()
    );
    if let (Some(run), Some(order)) = (&spec.graph_run, graph_order) {
        seed_graph_run(&store, run, &order)?;
        summary.push_str(&format!(
            "; succeeded graph run {} of workflow {} ({} nodes)",
            run.execution_id,
            run.workflow_id,
            order.len()
        ));
    }
    Ok(summary)
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match run(&args) {
        Ok(summary) => println!("{summary}"),
        Err(error) => {
            eprintln!("rsi-e2e-seed: {error}");
            std::process::exit(2);
        }
    }
}
