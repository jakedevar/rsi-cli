//! Graphs and topology commands (hub #3013, slice 11): graph review, the
//! recursive DAG browser, the ESP Square and the topology preview.
//!
//! Each daemon command validates its typed input, builds the exact request
//! the TUI sends (crates/rsi/src/client.rs; the builders below are pure and
//! unit-tested) and forwards one daemon method; the daemon validates again
//! and owns the authority. Three commands make no daemon call: the starter
//! templates and the executable-workflow preflight are client-side Rust in the
//! TUI (rsi_graph), so the desktop links the same crate instead of copying it.

use serde_json::{Map, Value, json};
use tauri::State;

use crate::{AppState, CmdResult, forward, parse_uuid};

/// Rows the DAG browser reads (types/recursive_dag.rs limits).
pub const RUN_LIMIT: u32 = 12;
pub const EVENT_LIMIT: u32 = 80;
pub const LIVE_LIMIT: u32 = 24;
pub const ARTIFACT_LIMIT: u32 = 24;
/// Cancellation reasons: trimmed, non-empty, at most 240 characters.
pub const REASON_LIMIT: usize = 240;
/// Who the desktop's control requests name (the TUI sends "rsi-tui" / "tui").
pub const REQUESTED_BY: &str = "rsi-desktop";
pub const OPERATOR: &str = "desktop";
/// A workflow definition the webview may send (the daemon caps it again).
pub const MAX_DEFINITION_BYTES: usize = 4 * 1024 * 1024;
const MAX_TEXT: usize = 65_536;
const MAX_CURSOR: usize = 1024;
const WORKFLOW_STAGES: &[&str] = &[
    "Research",
    "ResearchComplete",
    "Planning",
    "PlanComplete",
    "Implementing",
    "ImplementComplete",
    "Complete",
];
const RESOLVE_ACTIONS: &[&str] = &["inspect", "accept", "retry", "discard"];

type Built = Result<Value, String>;

fn uuid(field: &str, raw: &str) -> Result<String, String> {
    parse_uuid(field, raw)
}

fn opt_uuid(field: &str, raw: Option<&str>) -> Result<Value, String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Ok(json!(uuid(field, s)?)),
        None => Ok(Value::Null),
    }
}

fn bounded_text(field: &str, s: &str, max: usize) -> Result<String, String> {
    if s.len() > max || s.contains('\0') {
        return Err(format!("{field} is too long or contains NUL"));
    }
    Ok(s.to_string())
}

fn json_size(field: &str, v: &Value) -> Result<(), String> {
    let n = serde_json::to_vec(v).map_err(|e| e.to_string())?.len();
    if n > MAX_DEFINITION_BYTES {
        return Err(format!("{field} exceeds {MAX_DEFINITION_BYTES} bytes"));
    }
    Ok(())
}

// ---------- graph review: workflows ----------

/// `ListWorkflows` (client.rs list_workflows): `{}` or `{project_id}`.
pub fn list_workflows_params(project_id: Option<&str>) -> Built {
    match opt_uuid("project_id", project_id)? {
        Value::Null => Ok(json!({})),
        id => Ok(json!({ "project_id": id })),
    }
}

pub fn workflow_id_params(workflow_id: &str) -> Built {
    Ok(json!({ "workflow_id": uuid("workflow_id", workflow_id)? }))
}

pub fn execution_id_params(execution_id: &str) -> Built {
    Ok(json!({ "execution_id": uuid("execution_id", execution_id)? }))
}

/// `UpsertWorkflowDefinition {document}`: the row fields the TUI's
/// build_workflow_document sets, and a definition with nodes and edges.
pub fn upsert_params(document: &Value) -> Built {
    let o = document.as_object().ok_or("document must be an object")?;
    let row = o
        .get("workflow")
        .and_then(Value::as_object)
        .ok_or("document.workflow must be an object")?;
    let definition = o
        .get("definition")
        .ok_or("document.definition is required")?;
    check_definition(definition)?;
    let text = |k: &str| -> Result<String, String> {
        let s = row
            .get(k)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("workflow.{k} must be text"))?;
        bounded_text(&format!("workflow.{k}"), s, MAX_TEXT)
    };
    let stage = text("stage")?;
    if !WORKFLOW_STAGES.contains(&stage.as_str()) {
        return Err(format!(
            "workflow.stage must be one of {}",
            WORKFLOW_STAGES.join(", ")
        ));
    }
    let title = text("title")?;
    if title.trim().is_empty() {
        return Err("workflow.title is empty".into());
    }
    let artifact = match row.get("artifact_path") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(s)) => json!(bounded_text("workflow.artifact_path", s, 4096)?),
        Some(_) => return Err("workflow.artifact_path must be text or null".into()),
    };
    let project = opt_uuid(
        "workflow.project_id",
        row.get("project_id").and_then(Value::as_str),
    )?;
    Ok(json!({
        "document": {
            "workflow": {
                "id": uuid("workflow.id", &text("id")?)?,
                "title": title,
                "stage": stage,
                "artifact_path": artifact,
                "project_id": project,
                "created_at": text("created_at")?,
                "updated_at": text("updated_at")?,
            },
            "definition": definition,
        }
    }))
}

fn check_definition(definition: &Value) -> Result<(), String> {
    let d = definition
        .as_object()
        .ok_or("the workflow definition must be an object")?;
    if !d.get("nodes").is_some_and(Value::is_array) || !d.get("edges").is_some_and(Value::is_array)
    {
        return Err("the workflow definition needs nodes and edges lists".into());
    }
    json_size("the workflow definition", definition)
}

/// `ExecuteWorkflow` for the graph review's `r` (overlay/graph.rs
/// run_graph_draft): no input, not a dry run, no working dir, no parent.
pub fn execute_workflow_params(
    workflow_id: &str,
    workflow: &Value,
    project_id: Option<&str>,
) -> Built {
    check_definition(workflow)?;
    Ok(json!({
        "workflow_id": uuid("workflow_id", workflow_id)?,
        "workflow": workflow,
        "input": Value::Null,
        "dry_run": false,
        "project_id": opt_uuid("project_id", project_id)?,
        "working_dir": Value::Null,
        "parent_id": Value::Null,
    }))
}

/// `UpdateSessionWorkflow` for the graph review's `r` (overlay/graph.rs
/// run_graph_draft, client.rs update_session_workflow): link the selected
/// session to the workflow it just ran. The TUI only ever sends `Some`.
pub fn bind_session_workflow_params(session_id: &str, workflow_id: &str) -> Built {
    Ok(json!({
        "session_id": uuid("session_id", session_id)?,
        "workflow_id": uuid("workflow_id", workflow_id)?,
    }))
}

// ---------- graph review: bridged recursive graphs ----------

/// `ListRecursiveTaskGraphs`. The `R` picker sends the TUI's default params
/// (`include_quarantined: false`); the DAG browser asks for quarantined ones too.
pub fn list_recursive_params(project_id: Option<&str>, include_quarantined: bool) -> Built {
    let mut m = Map::new();
    let project = opt_uuid("project_id", project_id)?;
    m.insert("project_id".into(), project);
    m.insert("include_quarantined".into(), json!(include_quarantined));
    Ok(Value::Object(m))
}

pub fn graph_id_params(graph_id: &str) -> Built {
    Ok(json!({ "graph_id": uuid("graph_id", graph_id)? }))
}

pub fn edit_instructions_params(graph_id: &str, task_id: &str, instructions: &str) -> Built {
    Ok(json!({
        "graph_id": uuid("graph_id", graph_id)?,
        "task_id": uuid("task_id", task_id)?,
        "instructions": bounded_text("instructions", instructions, MAX_TEXT)?,
    }))
}

fn strategy(field: &str, v: Option<&str>) -> Result<Value, String> {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Ok(json!(bounded_text(field, s, MAX_TEXT)?)),
        None => Ok(Value::Null),
    }
}

/// `EditRecursiveNodeSettings`: a blank strategy clears it (sent as null).
pub fn edit_settings_params(
    graph_id: &str,
    task_id: &str,
    integration: Option<&str>,
    verification: Option<&str>,
) -> Built {
    Ok(json!({
        "graph_id": uuid("graph_id", graph_id)?,
        "task_id": uuid("task_id", task_id)?,
        "integration_strategy": strategy("integration_strategy", integration)?,
        "verification_strategy": strategy("verification_strategy", verification)?,
    }))
}

// ---------- topologies ----------

/// `ListTopologies {name_prefix}` (client.rs list_topologies).
pub fn list_topologies_params(name_prefix: Option<&str>) -> Built {
    match name_prefix {
        Some(p) => Ok(json!({ "name_prefix": bounded_text("name_prefix", p, 256)? })),
        None => Ok(json!({ "name_prefix": Value::Null })),
    }
}

pub fn topology_id_params(id: &str) -> Built {
    Ok(json!({ "id": uuid("id", id)? }))
}

/// `ExecuteTopology` (client.rs execute_topology): `inputs` is `{}` for `gR`
/// and `{issue: N}` for `:topology run`.
pub fn execute_topology_params(
    topology_id: &str,
    project_id: Option<&str>,
    issue: Option<u64>,
    parent_id: Option<&str>,
) -> Built {
    let inputs = match issue {
        Some(n) if n >= 1 => json!({ "issue": n }),
        Some(_) => return Err("issue must be a positive Issue number".into()),
        None => json!({}),
    };
    Ok(json!({
        "topology_id": uuid("topology_id", topology_id)?,
        "project_id": opt_uuid("project_id", project_id)?,
        "inputs": inputs,
        "parent_id": opt_uuid("parent_id", parent_id)?,
    }))
}

/// `ResolveTopologyAttempt` (#634; the daemon denies unknown fields). The
/// idempotency key binds the CAS version, so a repeated command replays; it
/// is the TUI's key, so a repeat from either client replays too.
pub fn resolve_params(
    execution_id: &str,
    attempt_id: &str,
    action: &str,
    expected_row_version: i64,
    confirm_preserved_commit: Option<&str>,
) -> Built {
    if !RESOLVE_ACTIONS.contains(&action) {
        return Err(format!(
            "action must be one of {}",
            RESOLVE_ACTIONS.join(", ")
        ));
    }
    if expected_row_version < 1 {
        return Err("expected_row_version must be positive".into());
    }
    let execution = uuid("execution_id", execution_id)?;
    let attempt = uuid("attempt_id", attempt_id)?;
    let mut m = Map::new();
    m.insert(
        "idempotency_key".into(),
        json!(format!(
            "tui:{execution}:{attempt}:{action}:{expected_row_version}"
        )),
    );
    m.insert("execution_id".into(), json!(execution));
    m.insert("attempt_id".into(), json!(attempt));
    m.insert("action".into(), json!(action));
    m.insert("expected_row_version".into(), json!(expected_row_version));
    if let Some(c) = confirm_preserved_commit
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        if c.len() > 64 || !c.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err("the preserved commit must be a hex commit id".into());
        }
        m.insert("confirm_preserved_commit".into(), json!(c));
    }
    Ok(Value::Object(m))
}

// ---------- recursive DAG browser reads ----------

pub fn scheduler_runs_params(graph_id: &str) -> Built {
    Ok(
        json!({ "graph_id": uuid("graph_id", graph_id)?, "limit": RUN_LIMIT, "include_terminal": true }),
    )
}

pub fn run_id_params(run_id: &str) -> Built {
    Ok(json!({ "run_id": uuid("run_id", run_id)? }))
}

pub fn run_events_params(run_id: &str) -> Built {
    Ok(json!({ "run_id": uuid("run_id", run_id)?, "limit": EVENT_LIMIT }))
}

pub fn cancellations_params(graph_id: &str) -> Built {
    Ok(json!({ "graph_id": uuid("graph_id", graph_id)?, "limit": LIVE_LIMIT }))
}

pub fn stale_heartbeats_params(graph_id: &str) -> Built {
    Ok(json!({ "graph_id": uuid("graph_id", graph_id)?, "limit": LIVE_LIMIT }))
}

/// GetRecursiveLiveRecoveryStatusParams::default() with the graph set.
pub fn live_recovery_params(graph_id: &str) -> Built {
    Ok(json!({
        "graph_id": uuid("graph_id", graph_id)?,
        "include_recovery_pending": true,
        "include_deferred_graph": true,
    }))
}

pub fn live_attempts_params(graph_id: &str) -> Built {
    Ok(json!({
        "graph_id": uuid("graph_id", graph_id)?,
        "include_terminal": true,
        "include_status": true,
        "limit": LIVE_LIMIT,
    }))
}

pub fn validations_params(graph_id: &str) -> Built {
    Ok(
        json!({ "graph_id": uuid("graph_id", graph_id)?, "include_issues": true, "limit": LIVE_LIMIT }),
    )
}

pub fn artifacts_params(graph_id: &str, cursor: Option<&str>) -> Built {
    let mut m = Map::new();
    m.insert("graph_id".into(), json!(uuid("graph_id", graph_id)?));
    m.insert("limit".into(), json!(ARTIFACT_LIMIT));
    m.insert("include_total".into(), json!(false));
    if let Some(c) = cursor.filter(|c| !c.is_empty()) {
        m.insert(
            "cursor".into(),
            json!(bounded_text("cursor", c, MAX_CURSOR)?),
        );
    }
    Ok(Value::Object(m))
}

// ---------- recursive DAG browser controls (gated in the webview and by the daemon) ----------

fn max_steps(n: u64) -> Result<u32, String> {
    match u32::try_from(n) {
        Ok(v) if v >= 1 => Ok(v),
        _ => Err("max_steps must be a positive integer (runs are never unbounded)".into()),
    }
}

fn reason(raw: &str) -> Result<String, String> {
    let r = raw.trim();
    if r.is_empty() {
        return Err("a cancellation reason is required".into());
    }
    if r.chars().count() > REASON_LIMIT || r.contains('\0') {
        return Err(format!(
            "the reason must be {REASON_LIMIT} characters or fewer"
        ));
    }
    Ok(r.to_string())
}

/// `RunRecursiveFakeScheduler` (client.rs recursive_fake_scheduler_params).
pub fn fake_run_params(graph_id: &str, steps: u64) -> Built {
    Ok(json!({
        "graph_id": uuid("graph_id", graph_id)?,
        "max_steps": max_steps(steps)?,
        "operator": OPERATOR,
        "execution_mode": "fake",
    }))
}

/// `RunRecursiveLiveScheduler`: only the bound and the operator; every
/// provider, model and budget field is left to the daemon's defaults, as in the TUI.
pub fn live_run_params(graph_id: &str, steps: u64) -> Built {
    Ok(json!({
        "graph_id": uuid("graph_id", graph_id)?,
        "max_steps": max_steps(steps)?,
        "operator": OPERATOR,
    }))
}

pub fn cancel_graph_params(graph_id: &str, why: &str) -> Built {
    Ok(
        json!({ "graph_id": uuid("graph_id", graph_id)?, "reason": reason(why)?, "requested_by": REQUESTED_BY }),
    )
}

pub fn cancel_run_params(run_id: &str, why: &str) -> Built {
    Ok(
        json!({ "run_id": uuid("run_id", run_id)?, "reason": reason(why)?, "requested_by": REQUESTED_BY }),
    )
}

/// `ContinueRecursiveRecovery`: `time_budget_ms` omitted when not given (0 is allowed).
pub fn recover_params(max_graphs: u64, time_budget_ms: Option<u64>) -> Built {
    let graphs = match u32::try_from(max_graphs) {
        Ok(v) if v >= 1 => v,
        _ => return Err("max_graphs must be a positive integer".into()),
    };
    let mut m = Map::new();
    m.insert("max_graphs".into(), json!(graphs));
    if let Some(t) = time_budget_ms {
        m.insert("time_budget_ms".into(), json!(t));
    }
    Ok(Value::Object(m))
}

// ---------- ESP Square ----------

/// `SaveEspGame` (client.rs save_esp_game): a finished 12-round game.
pub fn esp_params(score: u64, rounds_played: u64, p_value: f64, round_details: &str) -> Built {
    if score > 12 || rounds_played > 12 || score > rounds_played {
        return Err("score and rounds_played must be 0..=12 with score <= rounds_played".into());
    }
    if !p_value.is_finite() || !(0.0..=1.0 + 1e-9).contains(&p_value) {
        return Err("p_value must be within 0..=1".into());
    }
    let details = bounded_text("round_details", round_details, 16 * 1024)?;
    if !serde_json::from_str::<Value>(&details).is_ok_and(|v| v.is_array()) {
        return Err("round_details must be a JSON list".into());
    }
    Ok(json!({
        "score": score,
        "rounds_played": rounds_played,
        "total_rounds": 12,
        "p_value": p_value,
        "round_details": details,
    }))
}

// ---------- no-daemon helpers (rsi_graph, the TUI's own code) ----------

/// The topology picker's starter templates, in the TUI's order.
pub fn starters() -> Value {
    Value::Array(
        rsi_graph::generate::templates::starter_templates()
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "label": t.label,
                    "description": t.description,
                    "use_cases": t.use_cases,
                })
            })
            .collect(),
    )
}

pub fn starter_workflow(name: &str) -> CmdResult {
    let wf = rsi_graph::generate::templates::build_starter_workflow(name)
        .ok_or_else(|| format!("starter template unavailable: {name}"))?;
    serde_json::to_value(&wf).map_err(|e| e.to_string())
}

/// rsi_graph::validate_executable_workflow, the TUI's run preflight.
pub fn validate(workflow: &Value) -> CmdResult {
    json_size("the workflow definition", workflow)?;
    let wf: rsi_graph::format::WorkflowDefinition = serde_json::from_value(workflow.clone())
        .map_err(|e| format!("workflow encode failed: {e}"))?;
    serde_json::to_value(rsi_graph::validate_executable_workflow(&wf)).map_err(|e| e.to_string())
}

// ---------- commands ----------

#[tauri::command]
pub async fn graph_capabilities(state: State<'_, AppState>) -> CmdResult {
    forward(&state, "GetDaemonCapabilities", Value::Null).await
}

#[tauri::command]
pub async fn graph_list_workflows(
    state: State<'_, AppState>,
    project_id: Option<String>,
) -> CmdResult {
    forward(
        &state,
        "ListWorkflows",
        list_workflows_params(project_id.as_deref())?,
    )
    .await
}

#[tauri::command]
pub async fn graph_get_workflow(state: State<'_, AppState>, workflow_id: String) -> CmdResult {
    forward(
        &state,
        "GetWorkflowDefinition",
        workflow_id_params(&workflow_id)?,
    )
    .await
}

#[tauri::command]
pub async fn graph_upsert_workflow(state: State<'_, AppState>, document: Value) -> CmdResult {
    forward(
        &state,
        "UpsertWorkflowDefinition",
        upsert_params(&document)?,
    )
    .await
}

#[tauri::command]
pub async fn graph_execute_workflow(
    state: State<'_, AppState>,
    workflow_id: String,
    workflow: Value,
    project_id: Option<String>,
) -> CmdResult {
    forward(
        &state,
        "ExecuteWorkflow",
        execute_workflow_params(&workflow_id, &workflow, project_id.as_deref())?,
    )
    .await
}

/// After a successful run, link the selected session to the workflow so the
/// graph review reopens on it (`UpdateSessionWorkflow`).
#[tauri::command]
pub async fn graph_bind_session_workflow(
    state: State<'_, AppState>,
    session_id: String,
    workflow_id: String,
) -> CmdResult {
    forward(
        &state,
        "UpdateSessionWorkflow",
        bind_session_workflow_params(&session_id, &workflow_id)?,
    )
    .await
}

#[tauri::command]
pub async fn graph_get_execution(state: State<'_, AppState>, execution_id: String) -> CmdResult {
    forward(
        &state,
        "GetWorkflowExecution",
        execution_id_params(&execution_id)?,
    )
    .await
}

#[tauri::command]
pub async fn graph_interrupt_execution(
    state: State<'_, AppState>,
    execution_id: String,
) -> CmdResult {
    forward(
        &state,
        "InterruptWorkflowExecution",
        execution_id_params(&execution_id)?,
    )
    .await
}

#[tauri::command]
pub async fn graph_list_recursive(
    state: State<'_, AppState>,
    project_id: Option<String>,
    include_quarantined: bool,
) -> CmdResult {
    forward(
        &state,
        "ListRecursiveTaskGraphs",
        list_recursive_params(project_id.as_deref(), include_quarantined)?,
    )
    .await
}

#[tauri::command]
pub async fn graph_recursive_as_workflow(
    state: State<'_, AppState>,
    graph_id: String,
) -> CmdResult {
    forward(
        &state,
        "GetRecursiveGraphAsWorkflow",
        graph_id_params(&graph_id)?,
    )
    .await
}

#[tauri::command]
pub async fn graph_edit_recursive_instructions(
    state: State<'_, AppState>,
    graph_id: String,
    task_id: String,
    instructions: String,
) -> CmdResult {
    forward(
        &state,
        "EditRecursiveNodeInstructions",
        edit_instructions_params(&graph_id, &task_id, &instructions)?,
    )
    .await
}

#[tauri::command]
pub async fn graph_edit_recursive_settings(
    state: State<'_, AppState>,
    graph_id: String,
    task_id: String,
    integration_strategy: Option<String>,
    verification_strategy: Option<String>,
) -> CmdResult {
    forward(
        &state,
        "EditRecursiveNodeSettings",
        edit_settings_params(
            &graph_id,
            &task_id,
            integration_strategy.as_deref(),
            verification_strategy.as_deref(),
        )?,
    )
    .await
}

#[tauri::command]
pub async fn graph_starters() -> CmdResult {
    Ok(starters())
}

#[tauri::command]
pub async fn graph_starter_workflow(name: String) -> CmdResult {
    starter_workflow(&name)
}

#[tauri::command]
pub async fn graph_validate(workflow: Value) -> CmdResult {
    validate(&workflow)
}

#[tauri::command]
pub async fn topology_list(state: State<'_, AppState>, name_prefix: Option<String>) -> CmdResult {
    forward(
        &state,
        "ListTopologies",
        list_topologies_params(name_prefix.as_deref())?,
    )
    .await
}

#[tauri::command]
pub async fn topology_get(state: State<'_, AppState>, id: String) -> CmdResult {
    forward(&state, "GetTopology", topology_id_params(&id)?).await
}

#[tauri::command]
pub async fn topology_execute(
    state: State<'_, AppState>,
    topology_id: String,
    project_id: Option<String>,
    issue: Option<u64>,
    parent_id: Option<String>,
) -> CmdResult {
    forward(
        &state,
        "ExecuteTopology",
        execute_topology_params(
            &topology_id,
            project_id.as_deref(),
            issue,
            parent_id.as_deref(),
        )?,
    )
    .await
}

#[tauri::command]
pub async fn topology_resolve(
    state: State<'_, AppState>,
    execution_id: String,
    attempt_id: String,
    action: String,
    expected_row_version: i64,
    confirm_preserved_commit: Option<String>,
) -> CmdResult {
    forward(
        &state,
        "ResolveTopologyAttempt",
        resolve_params(
            &execution_id,
            &attempt_id,
            &action,
            expected_row_version,
            confirm_preserved_commit.as_deref(),
        )?,
    )
    .await
}

#[tauri::command]
pub async fn dag_get_graph(state: State<'_, AppState>, graph_id: String) -> CmdResult {
    forward(&state, "GetRecursiveTaskGraph", graph_id_params(&graph_id)?).await
}

#[tauri::command]
pub async fn dag_operational_status(state: State<'_, AppState>, graph_id: String) -> CmdResult {
    forward(
        &state,
        "GetRecursiveDagOperationalStatus",
        graph_id_params(&graph_id)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_scheduler_runs(state: State<'_, AppState>, graph_id: String) -> CmdResult {
    forward(
        &state,
        "ListRecursiveSchedulerRuns",
        scheduler_runs_params(&graph_id)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_scheduler_run(state: State<'_, AppState>, run_id: String) -> CmdResult {
    forward(&state, "GetRecursiveSchedulerRun", run_id_params(&run_id)?).await
}

#[tauri::command]
pub async fn dag_run_events(state: State<'_, AppState>, run_id: String) -> CmdResult {
    forward(
        &state,
        "ListRecursiveSchedulerRunEvents",
        run_events_params(&run_id)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_cancellations(state: State<'_, AppState>, graph_id: String) -> CmdResult {
    forward(
        &state,
        "ListRecursiveCancellationRequests",
        cancellations_params(&graph_id)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_recovery_status(state: State<'_, AppState>, graph_id: String) -> CmdResult {
    forward(
        &state,
        "GetRecursiveRecoveryStatus",
        graph_id_params(&graph_id)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_stale_heartbeats(state: State<'_, AppState>, graph_id: String) -> CmdResult {
    forward(
        &state,
        "ListStaleRecursiveLiveAttemptHeartbeats",
        stale_heartbeats_params(&graph_id)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_live_recovery(state: State<'_, AppState>, graph_id: String) -> CmdResult {
    forward(
        &state,
        "GetRecursiveLiveRecoveryStatus",
        live_recovery_params(&graph_id)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_live_attempts(state: State<'_, AppState>, graph_id: String) -> CmdResult {
    forward(
        &state,
        "ListRecursiveLiveAttempts",
        live_attempts_params(&graph_id)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_validations(state: State<'_, AppState>, graph_id: String) -> CmdResult {
    forward(
        &state,
        "ListRecursiveLiveOutputValidationResults",
        validations_params(&graph_id)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_artifacts(
    state: State<'_, AppState>,
    graph_id: String,
    cursor: Option<String>,
) -> CmdResult {
    forward(
        &state,
        "ListRecursiveExecutionArtifactSummaries",
        artifacts_params(&graph_id, cursor.as_deref())?,
    )
    .await
}

#[tauri::command]
pub async fn dag_fake_run(
    state: State<'_, AppState>,
    graph_id: String,
    max_steps: u64,
) -> CmdResult {
    forward(
        &state,
        "RunRecursiveFakeScheduler",
        fake_run_params(&graph_id, max_steps)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_live_run(
    state: State<'_, AppState>,
    graph_id: String,
    max_steps: u64,
) -> CmdResult {
    forward(
        &state,
        "RunRecursiveLiveScheduler",
        live_run_params(&graph_id, max_steps)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_cancel_graph(
    state: State<'_, AppState>,
    graph_id: String,
    reason: String,
) -> CmdResult {
    forward(
        &state,
        "RequestRecursiveGraphCancellation",
        cancel_graph_params(&graph_id, &reason)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_cancel_run(
    state: State<'_, AppState>,
    run_id: String,
    reason: String,
) -> CmdResult {
    forward(
        &state,
        "RequestRecursiveSchedulerRunCancellation",
        cancel_run_params(&run_id, &reason)?,
    )
    .await
}

#[tauri::command]
pub async fn dag_continue_recovery(
    state: State<'_, AppState>,
    max_graphs: u64,
    time_budget_ms: Option<u64>,
) -> CmdResult {
    forward(
        &state,
        "ContinueRecursiveRecovery",
        recover_params(max_graphs, time_budget_ms)?,
    )
    .await
}

#[tauri::command]
pub async fn esp_save_game(
    state: State<'_, AppState>,
    score: u64,
    rounds_played: u64,
    p_value: f64,
    round_details: String,
) -> CmdResult {
    forward(
        &state,
        "SaveEspGame",
        esp_params(score, rounds_played, p_value, &round_details)?,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: &str = "0b7c8a52-5f63-4a9e-9d27-3c1a9a5b6e10";
    const T: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";

    fn wf() -> Value {
        json!({ "version": "1.0", "name": "W", "nodes": [], "edges": [] })
    }

    #[test]
    fn workflow_requests_are_the_tuis() {
        assert_eq!(list_workflows_params(None).unwrap(), json!({}));
        assert_eq!(
            list_workflows_params(Some(G)).unwrap(),
            json!({ "project_id": G })
        );
        assert_eq!(workflow_id_params(G).unwrap(), json!({ "workflow_id": G }));
        assert_eq!(
            execute_workflow_params(G, &wf(), Some(T)).unwrap(),
            json!({
                "workflow_id": G, "workflow": wf(), "input": null, "dry_run": false,
                "project_id": T, "working_dir": null, "parent_id": null,
            })
        );
        assert!(execute_workflow_params(G, &json!({ "nodes": [] }), None).is_err());
        assert!(execute_workflow_params("nope", &wf(), None).is_err());
        assert_eq!(
            execution_id_params(G).unwrap(),
            json!({ "execution_id": G })
        );
    }

    #[test]
    fn upsert_rebuilds_the_document_and_refuses_bad_rows() {
        let doc = json!({
            "workflow": {
                "id": G, "title": "Pipeline", "stage": "Planning", "artifact_path": null,
                "project_id": T, "created_at": "2026-10-18T00:00:00Z", "updated_at": "2026-10-18T00:00:01Z",
                "extra": "dropped",
            },
            "definition": wf(),
        });
        let p = upsert_params(&doc).unwrap();
        assert_eq!(p["document"]["workflow"]["id"], json!(G));
        assert_eq!(p["document"]["workflow"]["project_id"], json!(T));
        assert!(p["document"]["workflow"].get("extra").is_none());
        assert_eq!(p["document"]["definition"], wf());
        let mut bad = doc.clone();
        bad["workflow"]["stage"] = json!("Bogus");
        assert!(upsert_params(&bad).is_err());
        let mut bad = doc.clone();
        bad["workflow"]["title"] = json!("  ");
        assert!(upsert_params(&bad).is_err());
        let mut bad = doc;
        bad["definition"] = json!("text");
        assert!(upsert_params(&bad).is_err());
    }

    #[test]
    fn recursive_graph_requests_match_the_client() {
        assert_eq!(
            list_recursive_params(None, false).unwrap(),
            json!({ "project_id": null, "include_quarantined": false })
        );
        assert_eq!(
            list_recursive_params(Some(G), true).unwrap(),
            json!({ "project_id": G, "include_quarantined": true })
        );
        assert_eq!(graph_id_params(G).unwrap(), json!({ "graph_id": G }));
        assert_eq!(
            edit_instructions_params(G, T, "do it").unwrap(),
            json!({ "graph_id": G, "task_id": T, "instructions": "do it" })
        );
        assert_eq!(
            edit_settings_params(G, T, Some(" merge "), Some("")).unwrap(),
            json!({ "graph_id": G, "task_id": T, "integration_strategy": "merge", "verification_strategy": null })
        );
    }

    #[test]
    fn topology_requests_match_the_client() {
        assert_eq!(
            list_topologies_params(None).unwrap(),
            json!({ "name_prefix": null })
        );
        assert_eq!(topology_id_params(G).unwrap(), json!({ "id": G }));
        assert_eq!(
            execute_topology_params(G, Some(T), None, Some(T)).unwrap(),
            json!({ "topology_id": G, "project_id": T, "inputs": {}, "parent_id": T })
        );
        assert_eq!(
            execute_topology_params(G, None, Some(61), None).unwrap()["inputs"],
            json!({ "issue": 61 })
        );
        assert!(execute_topology_params(G, None, Some(0), None).is_err());
    }

    #[test]
    fn resolve_binds_the_key_to_the_row_version() {
        let p = resolve_params(G, T, "retry", 7, None).unwrap();
        assert_eq!(
            p,
            json!({
                "execution_id": G, "attempt_id": T, "action": "retry",
                "expected_row_version": 7, "idempotency_key": format!("tui:{G}:{T}:retry:7"),
            })
        );
        assert!(p.get("confirm_preserved_commit").is_none());
        assert_eq!(
            resolve_params(G, T, "discard", 7, Some("abc123")).unwrap()["confirm_preserved_commit"],
            json!("abc123")
        );
        assert!(resolve_params(G, T, "nuke", 7, None).is_err());
        assert!(resolve_params(G, T, "accept", 0, None).is_err());
        assert!(resolve_params(G, T, "discard", 7, Some("not hex!")).is_err());
    }

    #[test]
    fn dag_reads_use_the_tuis_limits() {
        assert_eq!(
            scheduler_runs_params(G).unwrap(),
            json!({ "graph_id": G, "limit": 12, "include_terminal": true })
        );
        assert_eq!(run_id_params(G).unwrap(), json!({ "run_id": G }));
        assert_eq!(
            run_events_params(G).unwrap(),
            json!({ "run_id": G, "limit": 80 })
        );
        assert_eq!(
            cancellations_params(G).unwrap(),
            json!({ "graph_id": G, "limit": 24 })
        );
        assert_eq!(
            stale_heartbeats_params(G).unwrap(),
            json!({ "graph_id": G, "limit": 24 })
        );
        assert_eq!(
            live_recovery_params(G).unwrap(),
            json!({ "graph_id": G, "include_recovery_pending": true, "include_deferred_graph": true })
        );
        assert_eq!(
            live_attempts_params(G).unwrap(),
            json!({ "graph_id": G, "include_terminal": true, "include_status": true, "limit": 24 })
        );
        assert_eq!(
            validations_params(G).unwrap(),
            json!({ "graph_id": G, "include_issues": true, "limit": 24 })
        );
        assert_eq!(
            artifacts_params(G, None).unwrap(),
            json!({ "graph_id": G, "limit": 24, "include_total": false })
        );
        assert_eq!(
            artifacts_params(G, Some("c1")).unwrap()["cursor"],
            json!("c1")
        );
    }

    #[test]
    fn dag_controls_are_bounded_and_attributed() {
        assert_eq!(
            fake_run_params(G, 5).unwrap(),
            json!({ "graph_id": G, "max_steps": 5, "operator": "desktop", "execution_mode": "fake" })
        );
        assert!(fake_run_params(G, 0).is_err());
        assert!(live_run_params(G, u64::from(u32::MAX) + 1).is_err());
        assert_eq!(
            live_run_params(G, 3).unwrap(),
            json!({ "graph_id": G, "max_steps": 3, "operator": "desktop" })
        );
        assert_eq!(
            cancel_graph_params(G, "  stop it ").unwrap(),
            json!({ "graph_id": G, "reason": "stop it", "requested_by": "rsi-desktop" })
        );
        assert!(cancel_run_params(G, "   ").is_err());
        assert!(cancel_run_params(G, &"x".repeat(241)).is_err());
        assert!(cancel_run_params(G, &"x".repeat(240)).is_ok());
        assert_eq!(recover_params(2, None).unwrap(), json!({ "max_graphs": 2 }));
        assert_eq!(
            recover_params(2, Some(0)).unwrap(),
            json!({ "max_graphs": 2, "time_budget_ms": 0 })
        );
        assert!(recover_params(0, None).is_err());
    }

    #[test]
    fn esp_save_is_a_finished_game() {
        let p = esp_params(
            3,
            12,
            0.2,
            r#"[{"pick":1,"target":1,"hit":true,"guess_ms":10}]"#,
        )
        .unwrap();
        assert_eq!(p["total_rounds"], json!(12));
        assert_eq!(p["score"], json!(3));
        assert!(esp_params(13, 12, 0.1, "[]").is_err());
        assert!(esp_params(3, 2, 0.1, "[]").is_err());
        assert!(esp_params(1, 12, 1.5, "[]").is_err());
        assert!(esp_params(1, 12, 0.5, "{}").is_err());
    }

    #[test]
    fn starters_and_validation_come_from_rsi_graph() {
        let list = starters();
        let names: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.first(), Some(&"horizontal"));
        assert!(names.contains(&"pingpong"));
        for name in &names {
            let wf = starter_workflow(name).unwrap();
            assert!(
                wf["nodes"].as_array().is_some_and(|n| !n.is_empty()),
                "{name}"
            );
            let report = validate(&wf).unwrap();
            assert!(report.get("executable").is_some(), "{name}");
        }
        assert!(starter_workflow("nope").is_err());
        assert!(validate(&json!({ "nodes": "x" })).is_err());
    }

    #[test]
    fn session_workflow_binding_is_the_tuis_request() {
        assert_eq!(
            bind_session_workflow_params(T, G).unwrap(),
            json!({ "session_id": T, "workflow_id": G })
        );
        // Ids are trimmed and normalised; anything else is refused before the daemon.
        assert_eq!(
            bind_session_workflow_params(&format!(" {} ", T.to_uppercase()), G).unwrap(),
            json!({ "session_id": T, "workflow_id": G })
        );
        assert!(
            bind_session_workflow_params("nope", G)
                .unwrap_err()
                .contains("session_id")
        );
        assert!(
            bind_session_workflow_params(T, "")
                .unwrap_err()
                .contains("workflow_id")
        );
    }

    /// The layout tests (tests/graphs.test.mjs) lay out the real starter
    /// templates from this fixture; it must match rsi_graph. Regenerate with
    /// `RSI_DESKTOP_WRITE_FIXTURES=1 cargo test` in desktop/src-tauri.
    #[test]
    fn starter_fixture_is_current() {
        let mut all = Map::new();
        for t in rsi_graph::generate::templates::starter_templates() {
            all.insert(t.name.to_string(), starter_workflow(t.name).unwrap());
        }
        let want = serde_json::to_string_pretty(&Value::Object(all)).unwrap() + "\n";
        // cargo runs a package's tests from its directory (desktop/src-tauri).
        let path = std::path::Path::new("../tests/fixtures/graph-starters.json");
        if std::env::var_os("RSI_DESKTOP_WRITE_FIXTURES").is_some() {
            std::fs::write(path, &want).unwrap();
        }
        let have = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            have == want,
            "tests/fixtures/graph-starters.json is stale; rerun with RSI_DESKTOP_WRITE_FIXTURES=1"
        );
    }
}
