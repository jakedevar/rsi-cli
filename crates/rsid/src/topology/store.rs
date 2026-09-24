//! Durable rows for the topology executor (#634, plan §2.1–§2.2).
//!
//! Every mutation runs in one IMMEDIATE transaction that also bumps the
//! execution `row_version` and appends a `topology_events` row. The event
//! payload carries the exact `GraphExecutionUpdate` published on the bus, so
//! the TUI snapshot replays from durable rows after any restart.

use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::types::{
    GraphExecutionUpdate, WorkflowExecutionSnapshot, WorkflowExecutionStatus,
    WorkflowNodeExecutionState,
};
use rsi_graph::format::WorkflowDefinition;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::Value;
use sha2::Digest;
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::topology::custody::TopologyCustodyPlan;
use crate::topology::graph::RegionProgress;

/// Namespace for deterministic attempt and session identities.
const TOPOLOGY_ID_NAMESPACE: Uuid = Uuid::from_u128(0x5b0f_6c2e_7d1a_4b39_9e60_2f8d_0c4a_1e77);
/// Output stored inline per attempt (plan §2.1).
pub(crate) const OUTPUT_INLINE_LIMIT: usize = 64 * 1024;
/// Upper bound on replayed updates in one snapshot.
const SNAPSHOT_UPDATE_LIMIT: i64 = 512;

pub(crate) fn now_text() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_time(text: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| DaemonError::Store("invalid topology timestamp".into()))
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|_| DaemonError::Store("invalid topology UUID".into()))
}

pub(crate) fn attempt_id(execution_id: Uuid, node: &str, iteration: u32, attempt: u32) -> Uuid {
    Uuid::new_v5(
        &TOPOLOGY_ID_NAMESPACE,
        format!("attempt:{execution_id}:{node}:{iteration}:{attempt}").as_bytes(),
    )
}

/// The node session id is minted with the attempt, before any launch, so a
/// recovery pass can find the session row an interrupted launch produced.
pub(crate) fn attempt_session_id(attempt_id: Uuid) -> Uuid {
    Uuid::new_v5(
        &TOPOLOGY_ID_NAMESPACE,
        format!("session:{attempt_id}").as_bytes(),
    )
}

pub(crate) fn attempt_dedup_key(
    execution_id: Uuid,
    node: &str,
    iteration: u32,
    attempt: u32,
) -> String {
    format!("topology.node:{execution_id}:{node}:{iteration}:{attempt}")
}

pub(crate) fn digest(text: &str) -> String {
    format!(
        "sha256:{}",
        hex::encode(sha2::Sha256::digest(text.as_bytes()))
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecutionStatus {
    Accepted,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    Blocked,
}

impl ExecutionStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "accepted" => Self::Accepted,
            "running" => Self::Running,
            "cancelling" => Self::Cancelling,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "blocked" => Self::Blocked,
            _ => {
                return Err(DaemonError::Store(
                    "invalid topology execution status".into(),
                ));
            }
        })
    }

    /// Settled: no further effect will ever be taken.
    pub(crate) const fn is_final(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub(crate) const fn wire(self) -> WorkflowExecutionStatus {
        match self {
            Self::Accepted => WorkflowExecutionStatus::Accepted,
            Self::Running | Self::Cancelling => WorkflowExecutionStatus::Running,
            Self::Succeeded => WorkflowExecutionStatus::Succeeded,
            Self::Failed => WorkflowExecutionStatus::Failed,
            Self::Cancelled => WorkflowExecutionStatus::Interrupted,
            Self::Blocked => WorkflowExecutionStatus::Blocked,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptStatus {
    Reserved,
    Launching,
    Running,
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
    Interrupted,
    Lost,
    /// On an untaken path (plan §3 dead-path skipping); never launched.
    Skipped,
}

impl AttemptStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Launching => "launching",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::Lost => "lost",
            Self::Skipped => "skipped",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "reserved" => Self::Reserved,
            "launching" => Self::Launching,
            "running" | "waiting" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            "blocked" => Self::Blocked,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            "lost" => Self::Lost,
            _ => return Err(DaemonError::Store("invalid topology attempt status".into())),
        })
    }

    pub(crate) const fn in_flight(self) -> bool {
        matches!(self, Self::Reserved | Self::Launching | Self::Running)
    }

    const fn node_state(self) -> WorkflowNodeExecutionState {
        match self {
            Self::Reserved | Self::Launching | Self::Running => WorkflowNodeExecutionState::Running,
            Self::Succeeded => WorkflowNodeExecutionState::Succeeded,
            Self::Failed | Self::Blocked | Self::Cancelled | Self::Interrupted | Self::Lost => {
                WorkflowNodeExecutionState::Failed
            }
            Self::Skipped => WorkflowNodeExecutionState::Skipped,
        }
    }
}

/// Failure classes (plan §2.5). Only the executor writes them.
pub(crate) mod failure {
    pub(crate) const SESSION_FAILED: &str = "session_failed";
    pub(crate) const TIMEOUT: &str = "timeout";
    pub(crate) const INTERRUPTED: &str = "interrupted";
    pub(crate) const LOST_BEFORE_SESSION: &str = "lost_before_session";
    pub(crate) const LOST_AFTER_SESSION: &str = "lost_after_session";
    pub(crate) const PRESERVED_WORK: &str = "preserved_work";
    pub(crate) const CUSTODY_REFUSED: &str = "custody_refused";
    pub(crate) const LAUNCH_REFUSED: &str = "launch_refused";
    pub(crate) const CANCELLED: &str = "cancelled";
    pub(crate) const DISCARDED: &str = "preserved_work_discarded";
    pub(crate) const HANDOFF_INVALID: &str = "handoff_invalid";
    pub(crate) const HANDOFF_BLOCKED: &str = "handoff_blocked";
    /// A catalog op changed its sandbox (plan §3.2); preserved, never re-run.
    pub(crate) const SANDBOX_MUTATED: &str = "sandbox_mutated";
    pub(crate) const EXIT_NONZERO: &str = "exit_nonzero";
    pub(crate) const GATE_ERROR: &str = "gate_error";
    /// Every incoming edge of the node was untaken.
    pub(crate) const DEAD_PATH: &str = "dead_path";

    /// Losses that are not the node's fault: bounded per instance, never
    /// charged against the execution's attempt cap.
    pub(crate) const INFRASTRUCTURE: [&str; 3] =
        [LOST_BEFORE_SESSION, LOST_AFTER_SESSION, INTERRUPTED];

    pub(crate) fn is_infrastructure(class: Option<&str>) -> bool {
        class.is_some_and(|class| INFRASTRUCTURE.contains(&class))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ExecutionRow {
    pub(crate) id: Uuid,
    pub(crate) workflow_id: Uuid,
    pub(crate) name: String,
    pub(crate) definition: WorkflowDefinition,
    pub(crate) custody_plan: TopologyCustodyPlan,
    pub(crate) project_id: Option<Uuid>,
    pub(crate) parent_session_id: Option<Uuid>,
    pub(crate) repo_root: PathBuf,
    pub(crate) base_commit: String,
    pub(crate) status: ExecutionStatus,
    pub(crate) blocked_reason: Option<Value>,
    pub(crate) input: Option<Value>,
    pub(crate) output: Option<Value>,
    pub(crate) error: Option<String>,
    pub(crate) row_version: i64,
    pub(crate) max_node_attempts: u32,
    pub(crate) deadline_at: Option<DateTime<Utc>>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) started_at: Option<DateTime<Utc>>,
    pub(crate) finished_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub(crate) struct AttemptRow {
    pub(crate) id: Uuid,
    pub(crate) node_id: String,
    pub(crate) iteration: u32,
    pub(crate) attempt_no: u32,
    pub(crate) status: AttemptStatus,
    pub(crate) dedup_key: String,
    pub(crate) session_id: Uuid,
    pub(crate) boot_id: Option<Uuid>,
    pub(crate) sandbox_root: Option<PathBuf>,
    pub(crate) base_commit: String,
    pub(crate) result_commit: Option<String>,
    pub(crate) input: Value,
    pub(crate) output: Option<Value>,
    pub(crate) failure_class: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) resolution: Option<String>,
    pub(crate) preserved_ref: Option<String>,
    pub(crate) preserved_commit: Option<String>,
    pub(crate) started_at: Option<DateTime<Utc>>,
    /// `session`, `command` or `gate`.
    pub(crate) node_kind: String,
    /// Command nodes: sandbox HEAD recorded before the op ran.
    pub(crate) pre_head: Option<String>,
    /// Command nodes: live process group of the op.
    pub(crate) process_group_id: Option<i32>,
}

impl AttemptRow {
    pub(crate) fn query(&self) -> &str {
        self.input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }
}

/// A new execution, validated and custody-planned before any row exists.
pub(crate) struct NewExecution {
    pub(crate) id: Uuid,
    pub(crate) topology_id: Option<Uuid>,
    pub(crate) workflow_id: Uuid,
    pub(crate) definition: WorkflowDefinition,
    pub(crate) custody_plan: TopologyCustodyPlan,
    pub(crate) project_id: Option<Uuid>,
    pub(crate) parent_session_id: Option<Uuid>,
    pub(crate) repo_root: PathBuf,
    pub(crate) base_commit: String,
    pub(crate) input: Option<Value>,
}

pub(crate) struct NewAttempt {
    pub(crate) node_id: String,
    pub(crate) iteration: u32,
    pub(crate) attempt_no: u32,
    pub(crate) base_commit: String,
    pub(crate) query: String,
    /// `session`, `command` or `gate` (plan §3); daemon-derived.
    pub(crate) node_kind: &'static str,
    /// Command nodes: the catalog op name and its daemon-derived class.
    pub(crate) catalog_op: Option<&'static str>,
    pub(crate) effect_class: Option<&'static str>,
}

/// Terminal (or blocked) outcome of one attempt.
#[derive(Default)]
pub(crate) struct Settlement {
    pub(crate) failure_class: Option<&'static str>,
    pub(crate) error: Option<String>,
    pub(crate) result_commit: Option<String>,
    pub(crate) pin_ref: Option<String>,
    pub(crate) output: Option<Value>,
    pub(crate) preserved_ref: Option<String>,
    pub(crate) preserved_commit: Option<String>,
    pub(crate) preserved_paths_digest: Option<String>,
}

fn immediate(store: &Store) -> Result<Transaction<'_>> {
    Ok(Transaction::new_unchecked(
        &store.conn,
        TransactionBehavior::Immediate,
    )?)
}

/// Snapshot of the fields an event update projects.
struct ExecutionHeader {
    id: Uuid,
    workflow_id: Uuid,
    topology_id: Option<Uuid>,
    status: ExecutionStatus,
}

fn header(conn: &Connection, id: Uuid) -> Result<ExecutionHeader> {
    let (workflow_id, topology_id, status): (String, Option<String>, String) = conn
        .query_row(
            "SELECT workflow_id,topology_id,status FROM topology_executions WHERE id=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .ok_or_else(|| DaemonError::InvalidParam(format!("topology execution not found: {id}")))?;
    Ok(ExecutionHeader {
        id,
        workflow_id: parse_uuid(&workflow_id)?,
        topology_id: topology_id.as_deref().map(parse_uuid).transpose()?,
        status: ExecutionStatus::parse(&status)?,
    })
}

/// One durable progress event plus the bus update it projects to.
pub(crate) struct EventSpec<'a> {
    pub(crate) kind: &'a str,
    pub(crate) node_id: Option<&'a str>,
    pub(crate) attempt_id: Option<Uuid>,
    pub(crate) node_state: Option<WorkflowNodeExecutionState>,
    pub(crate) error: Option<String>,
    pub(crate) output_preview: Option<String>,
    pub(crate) detail: Value,
    pub(crate) actor_kind: &'a str,
}

impl<'a> EventSpec<'a> {
    pub(crate) const fn execution(kind: &'a str) -> Self {
        Self {
            kind,
            node_id: None,
            attempt_id: None,
            node_state: None,
            error: None,
            output_preview: None,
            detail: Value::Null,
            actor_kind: "executor",
        }
    }
}

fn append_event(
    conn: &Connection,
    execution_id: Uuid,
    spec: EventSpec<'_>,
) -> Result<GraphExecutionUpdate> {
    let header = header(conn, execution_id)?;
    let sequence: i64 = conn.query_row(
        "SELECT COALESCE(MAX(execution_seq),0)+1 FROM topology_events WHERE execution_id=?1",
        [execution_id.to_string()],
        |row| row.get(0),
    )?;
    let now = Utc::now();
    let update = GraphExecutionUpdate {
        execution_id: header.id,
        workflow_id: header.workflow_id,
        node_id: spec.node_id.map(str::to_owned),
        sequence: u64::try_from(sequence).unwrap_or_default(),
        status: header.status.wire(),
        node_state: spec.node_state,
        // Exactly one `finished` update per execution: the terminal
        // transition. Bookkeeping after settlement (cleanup) must not
        // re-trigger `finished` consumers such as the chain driver.
        finished: header.status.is_final() && spec.kind == header.status.as_str(),
        error: spec.error,
        output_preview: spec.output_preview,
        updated_at: now,
    };
    let payload = serde_json::json!({ "update": update, "detail": spec.detail });
    conn.execute(
        "INSERT INTO topology_events (execution_id,execution_seq,topology_id,node_id,attempt_id,kind,actor_kind,actor_session_id,payload_json,created_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,NULL,?8,?9)",
        params![
            execution_id.to_string(),
            sequence,
            header.topology_id.map(|id| id.to_string()),
            spec.node_id,
            spec.attempt_id.map(|id| id.to_string()),
            spec.kind,
            spec.actor_kind,
            payload.to_string(),
            now.to_rfc3339_opts(SecondsFormat::Nanos, true),
        ],
    )?;
    Ok(update)
}

fn bump(conn: &Connection, execution_id: Uuid) -> Result<()> {
    conn.execute(
        "UPDATE topology_executions SET row_version=row_version+1,updated_at=?2 WHERE id=?1",
        params![execution_id.to_string(), now_text()],
    )?;
    Ok(())
}

pub(crate) fn insert_execution(store: &Store, new: &NewExecution) -> Result<GraphExecutionUpdate> {
    let definition = serde_json::to_string(&new.definition)?;
    let custody_plan = serde_json::to_string(&new.custody_plan)?;
    let now = now_text();
    let tx = immediate(store)?;
    tx.execute(
        "INSERT INTO topology_executions (id,topology_id,topology_revision,topology_name_snapshot,workflow_id,definition_json,definition_digest,project_id,parent_session_id,requested_by_kind,repo_root,base_ref,base_commit,custody_plan_json,status,input_json,max_node_attempts,created_at,updated_at) \
         VALUES (?1,(SELECT id FROM topologies WHERE id=?2),NULL,?3,?4,?5,?6,?7,?8,'operator',?9,'refs/remotes/origin/rolling',?10,?11,'accepted',?12,?13,?14,?14)",
        params![
            new.id.to_string(),
            new.topology_id.map(|id| id.to_string()),
            new.definition.name,
            new.workflow_id.to_string(),
            definition,
            digest(&definition),
            new.project_id.map(|id| id.to_string()),
            new.parent_session_id.map(|id| id.to_string()),
            new.repo_root.display().to_string(),
            new.base_commit,
            custody_plan,
            new.input.as_ref().map(Value::to_string),
            i64::from(crate::topology::graph::MAX_NODE_ATTEMPTS_PER_EXECUTION),
            now,
        ],
    )?;
    let update = append_event(&tx, new.id, EventSpec::execution("accepted"))?;
    tx.commit()?;
    Ok(update)
}

const EXECUTION_COLUMNS: &str = "id,topology_id,workflow_id,topology_name_snapshot,definition_json,custody_plan_json,project_id,parent_session_id,repo_root,base_commit,status,blocked_reason_json,input_json,output_json,error,row_version,max_node_attempts,deadline_at,created_at,started_at,finished_at";

fn execution_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Vec<Option<String>>> {
    (0..21)
        .map(|index| match row.get_ref(index)? {
            rusqlite::types::ValueRef::Null => Ok(None),
            rusqlite::types::ValueRef::Integer(value) => Ok(Some(value.to_string())),
            _ => row.get::<_, String>(index).map(Some),
        })
        .collect()
}

fn required(columns: &[Option<String>], index: usize) -> Result<&str> {
    columns
        .get(index)
        .and_then(Option::as_deref)
        .ok_or_else(|| DaemonError::Store("missing topology execution column".into()))
}

fn optional_json(text: Option<&String>) -> Result<Option<Value>> {
    text.map(|text| serde_json::from_str(text).map_err(DaemonError::from))
        .transpose()
}

pub(crate) fn load_execution(store: &Store, id: Uuid) -> Result<Option<ExecutionRow>> {
    let columns = store
        .conn
        .query_row(
            &format!("SELECT {EXECUTION_COLUMNS} FROM topology_executions WHERE id=?1"),
            [id.to_string()],
            execution_from_row,
        )
        .optional()?;
    let Some(c) = columns else {
        return Ok(None);
    };
    let optional_uuid = |index: usize| c[index].as_deref().map(parse_uuid).transpose();
    let optional_time = |index: usize| c[index].as_deref().map(parse_time).transpose();
    Ok(Some(ExecutionRow {
        id: parse_uuid(required(&c, 0)?)?,
        workflow_id: parse_uuid(required(&c, 2)?)?,
        name: required(&c, 3)?.to_owned(),
        definition: serde_json::from_str(required(&c, 4)?)?,
        custody_plan: serde_json::from_str(required(&c, 5)?)?,
        project_id: optional_uuid(6)?,
        parent_session_id: optional_uuid(7)?,
        repo_root: PathBuf::from(required(&c, 8)?),
        base_commit: required(&c, 9)?.to_owned(),
        status: ExecutionStatus::parse(required(&c, 10)?)?,
        blocked_reason: optional_json(c[11].as_ref())?,
        input: optional_json(c[12].as_ref())?,
        output: optional_json(c[13].as_ref())?,
        error: c[14].clone(),
        row_version: required(&c, 15)?
            .parse()
            .map_err(|_| DaemonError::Store("invalid row_version".into()))?,
        max_node_attempts: required(&c, 16)?
            .parse()
            .map_err(|_| DaemonError::Store("invalid max_node_attempts".into()))?,
        deadline_at: optional_time(17)?,
        created_at: parse_time(required(&c, 18)?)?,
        started_at: optional_time(19)?,
        finished_at: optional_time(20)?,
    }))
}

const ATTEMPT_COLUMNS: &str = "id,node_id,iteration,attempt_no,status,dedup_key,session_id,boot_id,sandbox_root,base_commit,result_commit,pin_ref,input_json,output_json,failure_class,error,resolution,preserved_ref,preserved_commit,started_at,node_kind,pre_head,process_group_id";

fn attempt_from_row(row: &rusqlite::Row<'_>) -> Result<AttemptRow> {
    let text = |index: usize| row.get::<_, Option<String>>(index);
    let input: String = row.get(12)?;
    Ok(AttemptRow {
        id: parse_uuid(&row.get::<_, String>(0)?)?,
        node_id: row.get(1)?,
        iteration: row.get(2)?,
        attempt_no: row.get(3)?,
        status: AttemptStatus::parse(&row.get::<_, String>(4)?)?,
        dedup_key: row.get(5)?,
        session_id: parse_uuid(
            &text(6)?.ok_or_else(|| DaemonError::Store("attempt has no session id".into()))?,
        )?,
        boot_id: text(7)?.as_deref().map(parse_uuid).transpose()?,
        sandbox_root: text(8)?.map(PathBuf::from),
        base_commit: text(9)?.unwrap_or_default(),
        result_commit: text(10)?,
        input: serde_json::from_str(&input)?,
        output: text(13)?.as_deref().map(serde_json::from_str).transpose()?,
        failure_class: text(14)?,
        error: text(15)?,
        resolution: text(16)?,
        preserved_ref: text(17)?,
        preserved_commit: text(18)?,
        started_at: text(19)?.as_deref().map(parse_time).transpose()?,
        node_kind: row.get(20)?,
        pre_head: text(21)?,
        process_group_id: row.get(22)?,
    })
}

pub(crate) fn load_attempts(store: &Store, execution_id: Uuid) -> Result<Vec<AttemptRow>> {
    let mut statement = store.conn.prepare(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM topology_node_attempts WHERE execution_id=?1 \
         ORDER BY iteration,node_id,attempt_no"
    ))?;
    let mut rows = statement.query([execution_id.to_string()])?;
    let mut attempts = Vec::new();
    while let Some(row) = rows.next()? {
        attempts.push(attempt_from_row(row)?);
    }
    Ok(attempts)
}

pub(crate) fn load_attempt(store: &Store, attempt_id: Uuid) -> Result<Option<(Uuid, AttemptRow)>> {
    let mut statement = store.conn.prepare(&format!(
        "SELECT {ATTEMPT_COLUMNS},execution_id FROM topology_node_attempts WHERE id=?1"
    ))?;
    let mut rows = statement.query([attempt_id.to_string()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let execution_id = parse_uuid(&row.get::<_, String>(23)?)?;
    Ok(Some((execution_id, attempt_from_row(row)?)))
}

/// Recorded region decisions, as progress per SCC region index.
pub(crate) fn load_region_progress(
    store: &Store,
    execution_id: Uuid,
    regions: usize,
) -> Result<Vec<RegionProgress>> {
    let mut progress = vec![RegionProgress::default(); regions];
    let mut statement = store.conn.prepare(
        "SELECT json_extract(payload_json,'$.detail.region'),json_extract(payload_json,'$.detail.decision') \
         FROM topology_events WHERE execution_id=?1 AND kind='region_iteration' ORDER BY execution_seq",
    )?;
    let rows = statement.query_map([execution_id.to_string()], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (region, decision) = row?;
        let Some(entry) = usize::try_from(region)
            .ok()
            .and_then(|region| progress.get_mut(region))
        else {
            continue;
        };
        if entry.halted {
            continue;
        }
        if decision == "continue" {
            entry.current += 1;
        } else {
            entry.halted = true;
        }
    }
    Ok(progress)
}

pub(crate) fn record_region_decision(
    store: &Store,
    execution_id: Uuid,
    region: usize,
    iteration: u32,
    decision: &str,
) -> Result<GraphExecutionUpdate> {
    let tx = immediate(store)?;
    bump(&tx, execution_id)?;
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            detail: serde_json::json!({
                "region": region,
                "iteration": iteration,
                "decision": decision,
            }),
            output_preview: Some(format!(
                "loop region {region} iteration {iteration}: {decision}"
            )),
            ..EventSpec::execution("region_iteration")
        },
    )?;
    tx.commit()?;
    Ok(update)
}

/// Executions an executor must drive: accepted, running or cancelling, plus
/// blocked executions that still own in-flight attempts.
pub(crate) fn drivable_execution_ids(store: &Store, limit: usize) -> Result<Vec<Uuid>> {
    let mut statement = store.conn.prepare(
        "SELECT e.id FROM topology_executions e WHERE e.status IN ('accepted','running','cancelling') \
            OR (e.status='blocked' AND EXISTS (SELECT 1 FROM topology_node_attempts a \
                WHERE a.execution_id=e.id AND a.status IN ('reserved','launching','running','waiting'))) \
         ORDER BY e.created_at LIMIT ?1",
    )?;
    let ids = statement
        .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ids.iter().map(|id| parse_uuid(id)).collect()
}

/// Claim the execution lease for this daemon incarnation (crash fence).
pub(crate) fn claim_lease(store: &Store, execution_id: Uuid, boot_id: Uuid) -> Result<()> {
    store.conn.execute(
        "UPDATE topology_executions SET lease_owner=?2,lease_expires_at=NULL WHERE id=?1",
        params![execution_id.to_string(), boot_id.to_string()],
    )?;
    Ok(())
}

/// Reserve new attempts in one transaction: pre-minted session id and
/// deterministic dedup key per attempt, bounded by `max_node_attempts`. Only
/// the executor (or the audited retry resolution) increments `attempt_no`.
pub(crate) fn reserve_attempts(
    store: &Store,
    execution_id: Uuid,
    attempts: &[NewAttempt],
) -> Result<Vec<GraphExecutionUpdate>> {
    if attempts.is_empty() {
        return Ok(Vec::new());
    }
    let tx = immediate(store)?;
    let updates = insert_attempts(&tx, execution_id, attempts.iter().map(|a| (a, None)))?;
    bump(&tx, execution_id)?;
    tx.commit()?;
    Ok(updates)
}

/// Instances whose custody cannot resolve are recorded as failed attempts
/// that never reach an effect.
pub(crate) fn refuse_attempts(
    store: &Store,
    execution_id: Uuid,
    refused: &[(NewAttempt, String)],
) -> Result<Vec<GraphExecutionUpdate>> {
    if refused.is_empty() {
        return Ok(Vec::new());
    }
    let tx = immediate(store)?;
    let updates = insert_attempts(
        &tx,
        execution_id,
        refused
            .iter()
            .map(|(attempt, error)| (attempt, Some(error.as_str()))),
    )?;
    bump(&tx, execution_id)?;
    tx.commit()?;
    Ok(updates)
}

fn insert_attempts<'a>(
    tx: &Connection,
    execution_id: Uuid,
    attempts: impl ExactSizeIterator<Item = (&'a NewAttempt, Option<&'a str>)>,
) -> Result<Vec<GraphExecutionUpdate>> {
    let (status, stored_cap, definition, existing): (String, u32, String, i64) = tx.query_row(
        "SELECT status,max_node_attempts,definition_json,(SELECT count(*) FROM topology_node_attempts \
            WHERE execution_id=?1 AND (failure_class IS NULL OR failure_class NOT IN (?2,?3,?4))) \
         FROM topology_executions WHERE id=?1",
        params![
            execution_id.to_string(),
            failure::INFRASTRUCTURE[0],
            failure::INFRASTRUCTURE[1],
            failure::INFRASTRUCTURE[2],
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    // The cap derives from the immutable definition snapshot (the stored
    // column is CHECK-bounded at 64 by the released V129 DDL).
    let max_attempts = i64::from(
        crate::topology::graph::GraphShape::from_workflow(&serde_json::from_str(&definition)?)?
            .attempt_cap(stored_cap),
    );
    if !matches!(
        ExecutionStatus::parse(&status)?,
        ExecutionStatus::Accepted | ExecutionStatus::Running
    ) {
        return Err(DaemonError::InvalidParam(
            "topology execution no longer accepts new attempts".into(),
        ));
    }
    let requested = i64::try_from(attempts.len()).unwrap_or(i64::MAX);
    if existing.saturating_add(requested) > max_attempts {
        return Err(DaemonError::PolicyDenied(
            "topology execution exhausted max_node_attempts".into(),
        ));
    }
    let now = now_text();
    let mut updates = Vec::new();
    for (attempt, refusal) in attempts {
        let id = attempt_id(
            execution_id,
            &attempt.node_id,
            attempt.iteration,
            attempt.attempt_no,
        );
        let status = if refusal.is_some() {
            "failed"
        } else {
            "reserved"
        };
        tx.execute(
            "INSERT INTO topology_node_attempts (id,execution_id,node_id,iteration,attempt_no,node_kind,status,dedup_key,session_id,base_commit,input_json,failure_class,error,finished_at,created_at,updated_at,catalog_op,effect_class) \
             VALUES (?1,?2,?3,?4,?5,?15,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14,?16,?17)",
            params![
                id.to_string(),
                execution_id.to_string(),
                attempt.node_id,
                attempt.iteration,
                attempt.attempt_no,
                status,
                attempt_dedup_key(execution_id, &attempt.node_id, attempt.iteration, attempt.attempt_no),
                attempt_session_id(id).to_string(),
                attempt.base_commit,
                serde_json::json!({ "query": attempt.query }).to_string(),
                refusal.map(|_| failure::CUSTODY_REFUSED),
                refusal,
                refusal.map(|_| now.clone()),
                now,
                attempt.node_kind,
                attempt.catalog_op,
                attempt.effect_class,
            ],
        )?;
        updates.push(append_event(
            tx,
            execution_id,
            EventSpec {
                node_id: Some(&attempt.node_id),
                attempt_id: Some(id),
                node_state: Some(if refusal.is_some() {
                    WorkflowNodeExecutionState::Failed
                } else {
                    WorkflowNodeExecutionState::Running
                }),
                error: refusal.map(str::to_owned),
                detail: serde_json::json!({
                    "iteration": attempt.iteration,
                    "attempt_no": attempt.attempt_no,
                    "base_commit": attempt.base_commit,
                }),
                ..EventSpec::execution(if refusal.is_some() {
                    "node_refused"
                } else {
                    "node_reserved"
                })
            },
        )?);
    }
    Ok(updates)
}

/// `reserved` → `launching`, stamped with the launching incarnation.
pub(crate) fn mark_launching(store: &Store, attempt_id: Uuid, boot_id: Uuid) -> Result<bool> {
    let changed = store.conn.execute(
        "UPDATE topology_node_attempts SET status='launching',boot_id=?2,updated_at=?3 \
         WHERE id=?1 AND status IN ('reserved','launching')",
        params![attempt_id.to_string(), boot_id.to_string(), now_text()],
    )?;
    Ok(changed == 1)
}

pub(crate) fn mark_running(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    started_at: Option<DateTime<Utc>>,
) -> Result<GraphExecutionUpdate> {
    let tx = immediate(store)?;
    tx.execute(
        "UPDATE topology_node_attempts SET status='running',started_at=COALESCE(started_at,?2),updated_at=?3 \
         WHERE id=?1 AND status IN ('reserved','launching')",
        params![
            attempt.id.to_string(),
            started_at
                .unwrap_or_else(Utc::now)
                .to_rfc3339_opts(SecondsFormat::Nanos, true),
            now_text(),
        ],
    )?;
    bump(&tx, execution_id)?;
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(&attempt.node_id),
            attempt_id: Some(attempt.id),
            node_state: Some(WorkflowNodeExecutionState::Running),
            detail: serde_json::json!({
                "iteration": attempt.iteration,
                "attempt_no": attempt.attempt_no,
                "session_id": attempt.session_id,
            }),
            ..EventSpec::execution("node_running")
        },
    )?;
    tx.commit()?;
    Ok(update)
}

/// This incarnation observed the attempt's session alive and now owns it.
pub(crate) fn stamp_boot(store: &Store, attempt_id: Uuid, boot_id: Uuid) -> Result<()> {
    store.conn.execute(
        "UPDATE topology_node_attempts SET boot_id=?2,updated_at=?3 WHERE id=?1",
        params![attempt_id.to_string(), boot_id.to_string(), now_text()],
    )?;
    Ok(())
}

pub(crate) fn record_sandbox_root(
    store: &Store,
    attempt_id: Uuid,
    root: &std::path::Path,
) -> Result<()> {
    store.conn.execute(
        "UPDATE topology_node_attempts SET sandbox_root=?2,updated_at=?3 WHERE id=?1 AND sandbox_root IS NULL",
        params![attempt_id.to_string(), root.display().to_string(), now_text()],
    )?;
    Ok(())
}

/// Record a terminal (or blocked) attempt outcome and its progress event.
pub(crate) fn settle_attempt(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    status: AttemptStatus,
    settlement: &Settlement,
) -> Result<GraphExecutionUpdate> {
    let output = settlement.output.as_ref().map(Value::to_string);
    if output
        .as_ref()
        .is_some_and(|text| text.len() > OUTPUT_INLINE_LIMIT)
    {
        // Callers store larger outputs as path@commit; never drop silently.
        return Err(DaemonError::Store(
            "topology output exceeds the inline limit".into(),
        ));
    }
    let tx = immediate(store)?;
    let update = settle_in(
        &tx,
        execution_id,
        attempt,
        status,
        settlement,
        output.as_deref(),
    )?;
    tx.commit()?;
    Ok(update)
}

fn settle_in(
    tx: &Connection,
    execution_id: Uuid,
    attempt: &AttemptRow,
    status: AttemptStatus,
    settlement: &Settlement,
    output: Option<&str>,
) -> Result<GraphExecutionUpdate> {
    let changed = tx.execute(
        "UPDATE topology_node_attempts SET status=?2,failure_class=?3,error=?4,result_commit=?5,pin_ref=?6,\
            output_json=?7,output_digest=?8,preserved_ref=?9,preserved_commit=?10,preserved_paths_digest=?11,\
            finished_at=?12,updated_at=?12 \
         WHERE id=?1 AND status IN ('reserved','launching','running','waiting')",
        params![
            attempt.id.to_string(),
            status.as_str(),
            settlement.failure_class,
            settlement.error,
            settlement.result_commit,
            settlement.pin_ref,
            output,
            output.map(digest),
            settlement.preserved_ref,
            settlement.preserved_commit,
            settlement.preserved_paths_digest,
            now_text(),
        ],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "topology attempt was already settled".into(),
        ));
    }
    bump(tx, execution_id)?;
    let preview = settlement
        .output
        .as_ref()
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .and_then(|data| crate::session::graph_runner::preview_node_data(&data));
    append_event(
        tx,
        execution_id,
        EventSpec {
            node_id: Some(&attempt.node_id),
            attempt_id: Some(attempt.id),
            node_state: Some(status.node_state()),
            error: settlement.error.clone(),
            output_preview: preview.or_else(|| settlement.failure_class.map(str::to_owned)),
            detail: serde_json::json!({
                "iteration": attempt.iteration,
                "attempt_no": attempt.attempt_no,
                "status": status.as_str(),
                "failure_class": settlement.failure_class,
                "result_commit": settlement.result_commit,
                "preserved_commit": settlement.preserved_commit,
            }),
            ..EventSpec::execution("node_settled")
        },
    )
}

/// Record attempts that settle without an effect (gate results and
/// dead-path skips) in one transaction: reserved, then settled.
pub(crate) fn record_settled_attempts(
    store: &Store,
    execution_id: Uuid,
    settled: &[(NewAttempt, AttemptStatus, Settlement)],
) -> Result<Vec<GraphExecutionUpdate>> {
    if settled.is_empty() {
        return Ok(Vec::new());
    }
    let tx = immediate(store)?;
    let mut updates = insert_attempts(&tx, execution_id, settled.iter().map(|(a, ..)| (a, None)))?;
    for (attempt, status, settlement) in settled {
        let output = settlement.output.as_ref().map(Value::to_string);
        if output
            .as_ref()
            .is_some_and(|text| text.len() > OUTPUT_INLINE_LIMIT)
        {
            return Err(DaemonError::Store(
                "topology output exceeds the inline limit".into(),
            ));
        }
        let id = attempt_id(
            execution_id,
            &attempt.node_id,
            attempt.iteration,
            attempt.attempt_no,
        );
        let row = AttemptRow {
            id,
            node_id: attempt.node_id.clone(),
            iteration: attempt.iteration,
            attempt_no: attempt.attempt_no,
            status: AttemptStatus::Reserved,
            dedup_key: String::new(),
            session_id: attempt_session_id(id),
            boot_id: None,
            sandbox_root: None,
            base_commit: attempt.base_commit.clone(),
            result_commit: None,
            input: Value::Null,
            output: None,
            failure_class: None,
            error: None,
            resolution: None,
            preserved_ref: None,
            preserved_commit: None,
            started_at: None,
            node_kind: attempt.node_kind.to_owned(),
            pre_head: None,
            process_group_id: None,
        };
        updates.push(settle_in(
            &tx,
            execution_id,
            &row,
            *status,
            settlement,
            output.as_deref(),
        )?);
    }
    tx.commit()?;
    Ok(updates)
}

/// Command nodes: `reserved` → `launching` only while the daemon-wide
/// `topology_max_concurrent_build_nodes` cap has room. A re-entry of an
/// attempt that is already `launching` is always allowed (already counted).
pub(crate) fn claim_command_slot(
    store: &Store,
    attempt_id: Uuid,
    boot_id: Uuid,
    cap: u32,
) -> Result<bool> {
    let changed = store.conn.execute(
        "UPDATE topology_node_attempts SET status='launching',boot_id=?2,updated_at=?3 \
         WHERE id=?1 AND (status='launching' OR (status='reserved' AND \
           (SELECT count(*) FROM topology_node_attempts WHERE node_kind='command' \
              AND status IN ('launching','running')) < ?4))",
        params![attempt_id.to_string(), boot_id.to_string(), now_text(), cap],
    )?;
    Ok(changed == 1)
}

/// Command nodes: the fresh sandbox and its pre-op HEAD, recorded before
/// the op starts so a restart can check the postcondition.
pub(crate) fn record_command_sandbox(
    store: &Store,
    attempt_id: Uuid,
    root: &std::path::Path,
    pre_head: &str,
) -> Result<()> {
    store.conn.execute(
        "UPDATE topology_node_attempts SET sandbox_root=?2,pre_head=?3,updated_at=?4 WHERE id=?1",
        params![
            attempt_id.to_string(),
            root.display().to_string(),
            pre_head,
            now_text()
        ],
    )?;
    Ok(())
}

pub(crate) fn record_process_group(store: &Store, attempt_id: Uuid, pgid: i32) -> Result<()> {
    store.conn.execute(
        "UPDATE topology_node_attempts SET process_group_id=?2,updated_at=?3 WHERE id=?1",
        params![attempt_id.to_string(), pgid, now_text()],
    )?;
    Ok(())
}

/// Move the execution to `status` and append its transition event.
pub(crate) fn transition_execution(
    store: &Store,
    execution_id: Uuid,
    from: &[ExecutionStatus],
    to: ExecutionStatus,
    error: Option<String>,
    output: Option<&Value>,
    blocked_reason: Option<&Value>,
) -> Result<Option<GraphExecutionUpdate>> {
    let tx = immediate(store)?;
    let current = header(&tx, execution_id)?.status;
    if !from.contains(&current) {
        return Ok(None);
    }
    let now = now_text();
    tx.execute(
        "UPDATE topology_executions SET status=?2,error=COALESCE(?3,error),output_json=COALESCE(?4,output_json),\
            blocked_reason_json=?5,started_at=CASE WHEN ?2 IN ('running','cancelling') THEN COALESCE(started_at,?6) ELSE started_at END,\
            finished_at=CASE WHEN ?2 IN ('succeeded','failed','cancelled') THEN ?6 ELSE NULL END,\
            row_version=row_version+1,updated_at=?6 WHERE id=?1",
        params![
            execution_id.to_string(),
            to.as_str(),
            error,
            output.map(Value::to_string),
            blocked_reason.map(Value::to_string),
            now,
        ],
    )?;
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            error,
            output_preview: output.map(|value| {
                let text = value.to_string();
                if text.chars().count() > 120 {
                    format!("{}...", text.chars().take(120).collect::<String>())
                } else {
                    text
                }
            }),
            detail: serde_json::json!({ "from": current.as_str(), "to": to.as_str(), "blocked_reason": blocked_reason }),
            ..EventSpec::execution(to.as_str())
        },
    )?;
    tx.commit()?;
    Ok(Some(update))
}

/// Resolution bookkeeping shared by the operator RPC (plan §3.4).
pub(crate) struct ResolutionWrite<'a> {
    pub(crate) execution_id: Uuid,
    pub(crate) attempt: &'a AttemptRow,
    pub(crate) expected_row_version: i64,
    pub(crate) idempotency_key: &'a str,
    pub(crate) action: &'a str,
    pub(crate) attempt_status: AttemptStatus,
    pub(crate) resolution: Option<&'a str>,
    pub(crate) failure_class: Option<&'static str>,
    pub(crate) result_commit: Option<String>,
    pub(crate) pin_ref: Option<String>,
    pub(crate) retry: Option<NewAttempt>,
    pub(crate) event_kind: &'a str,
    pub(crate) detail: Value,
    /// Request fingerprint bound to the idempotency key.
    pub(crate) fingerprint: &'a str,
    /// Leave `blocked` now; a discard resumes only once its effects finish.
    pub(crate) resume: bool,
}

/// Outcome of the CAS/idempotency gate for a resolution action.
pub(crate) enum ResolutionGate {
    /// The same key already recorded this exact request.
    Replay,
    Fresh,
}

/// The idempotency key binds the whole request: attempt, action, observed
/// CAS version and the discard confirmation (digested), never the action
/// alone.
pub(crate) fn resolution_fingerprint(
    attempt_id: Uuid,
    action: &str,
    expected_row_version: i64,
    confirmation: Option<&str>,
) -> String {
    digest(&format!(
        "{attempt_id}|{action}|{expected_row_version}|{}",
        confirmation.map_or_else(
            || "null".to_owned(),
            |text| digest(&text.to_ascii_lowercase())
        )
    ))
}

/// Fast-path check before any work. The authoritative check is repeated
/// inside the recording transaction (`gate_in`), so two concurrent requests
/// with one key can never both record.
pub(crate) fn resolution_gate(
    store: &Store,
    execution_id: Uuid,
    idempotency_key: &str,
    fingerprint: &str,
) -> Result<ResolutionGate> {
    gate_in(&store.conn, execution_id, idempotency_key, fingerprint)
}

fn gate_in(
    conn: &Connection,
    execution_id: Uuid,
    idempotency_key: &str,
    fingerprint: &str,
) -> Result<ResolutionGate> {
    let prior: Option<Option<String>> = conn
        .query_row(
            "SELECT json_extract(payload_json,'$.detail.fingerprint') FROM topology_events \
             WHERE execution_id=?1 AND json_extract(payload_json,'$.detail.idempotency_key')=?2 \
             ORDER BY execution_seq LIMIT 1",
            params![execution_id.to_string(), idempotency_key],
            |row| row.get(0),
        )
        .optional()?;
    match prior {
        Some(Some(prior)) if prior == fingerprint => Ok(ResolutionGate::Replay),
        Some(_) => Err(super::resolve::resolution_error(
            "idempotency_conflict",
            "retry the original request or use a new idempotency_key",
            None,
            None,
        )),
        None => Ok(ResolutionGate::Fresh),
    }
}

/// Apply one resolution atomically under the execution row-version CAS.
pub(crate) fn write_resolution(store: &Store, write: ResolutionWrite<'_>) -> Result<Recorded> {
    let tx = immediate(store)?;
    // Key recording is atomic with the key check: a concurrent request that
    // recorded first makes this one a replay or an `idempotency_conflict`.
    if matches!(
        gate_in(
            &tx,
            write.execution_id,
            write.idempotency_key,
            write.fingerprint,
        )?,
        ResolutionGate::Replay
    ) {
        return Ok(Recorded::Replay);
    }
    let actual: i64 = tx.query_row(
        "SELECT row_version FROM topology_executions WHERE id=?1",
        [write.execution_id.to_string()],
        |row| row.get(0),
    )?;
    if actual != write.expected_row_version {
        return Err(super::resolve::resolution_error(
            "stale_row_version",
            "refresh the execution and retry with its current row_version",
            Some(write.expected_row_version),
            Some(actual),
        ));
    }
    let now = now_text();
    let changed = tx.execute(
        "UPDATE topology_node_attempts SET status=?2,resolution=COALESCE(?3,resolution),failure_class=COALESCE(?4,failure_class),\
            result_commit=COALESCE(?5,result_commit),pin_ref=COALESCE(?6,pin_ref),resolved_by_kind='operator',\
            resolved_at=?7,updated_at=?7 WHERE id=?1 AND status='blocked'",
        params![
            write.attempt.id.to_string(),
            write.attempt_status.as_str(),
            write.resolution,
            write.failure_class,
            write.result_commit,
            write.pin_ref,
            now,
        ],
    )?;
    if changed != 1 {
        return Err(super::resolve::resolution_error(
            "attempt_not_blocked",
            "refresh the execution; only a blocked preserved-work attempt resolves",
            None,
            None,
        ));
    }
    let mut detail = write.detail;
    if let Value::Object(map) = &mut detail {
        map.insert("action".into(), Value::String(write.action.to_owned()));
        map.insert(
            "idempotency_key".into(),
            Value::String(write.idempotency_key.to_owned()),
        );
        map.insert(
            "fingerprint".into(),
            Value::String(write.fingerprint.to_owned()),
        );
    }
    if write.resume {
        // The execution leaves `blocked`; the executor decides the next step.
        tx.execute(
            "UPDATE topology_executions SET status='running',blocked_reason_json=NULL,\
                row_version=row_version+1,updated_at=?2 WHERE id=?1 AND status='blocked'",
            params![write.execution_id.to_string(), now],
        )?;
    } else {
        bump(&tx, write.execution_id)?;
    }
    let mut updates = vec![append_event(
        &tx,
        write.execution_id,
        EventSpec {
            node_id: Some(&write.attempt.node_id),
            attempt_id: Some(write.attempt.id),
            node_state: Some(write.attempt_status.node_state()),
            detail,
            actor_kind: "operator",
            ..EventSpec::execution(write.event_kind)
        },
    )?];
    if let Some(retry) = &write.retry {
        updates.extend(insert_attempts(
            &tx,
            write.execution_id,
            std::iter::once((retry, None)),
        )?);
    }
    tx.commit()?;
    Ok(Recorded::Fresh(updates))
}

/// Outcome of recording one resolution request under its idempotency key.
pub(crate) enum Recorded {
    Fresh(Vec<GraphExecutionUpdate>),
    /// The same key and fingerprint were recorded first (possibly by a
    /// concurrent request); nothing was written.
    Replay,
}

/// Record an audited, non-mutating resolution read (`inspect`).
pub(crate) fn record_inspection(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    idempotency_key: &str,
    fingerprint: &str,
) -> Result<Recorded> {
    let tx = immediate(store)?;
    if matches!(
        gate_in(&tx, execution_id, idempotency_key, fingerprint)?,
        ResolutionGate::Replay
    ) {
        return Ok(Recorded::Replay);
    }
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(&attempt.node_id),
            attempt_id: Some(attempt.id),
            detail: serde_json::json!({
                "action": "inspect",
                "idempotency_key": idempotency_key,
                "fingerprint": fingerprint,
            }),
            actor_kind: "operator",
            ..EventSpec::execution("preserved_work_inspected")
        },
    )?;
    tx.commit()?;
    Ok(Recorded::Fresh(vec![update]))
}

/// Discards recorded but whose byte-destroying effects have not completed
/// (phase 1 of the two-phase discard: `resolution='discarded'` with the
/// preservation ref still recorded).
pub(crate) fn pending_discards(store: &Store, limit: usize) -> Result<Vec<(Uuid, Uuid)>> {
    let mut statement = store.conn.prepare(
        "SELECT execution_id,id FROM topology_node_attempts \
         WHERE resolution='discarded' AND preserved_ref IS NOT NULL LIMIT ?1",
    )?;
    let rows = statement
        .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.iter()
        .map(|(execution, attempt)| Ok((parse_uuid(execution)?, parse_uuid(attempt)?)))
        .collect()
}

/// Phase 2 of a discard: effects are done, so clear the ref record and let
/// the execution resume per `on_failure`. Idempotent.
pub(crate) fn finish_discard(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
) -> Result<Vec<GraphExecutionUpdate>> {
    let tx = immediate(store)?;
    let cleared = tx.execute(
        "UPDATE topology_node_attempts SET preserved_ref=NULL,updated_at=?2 \
         WHERE id=?1 AND resolution='discarded' AND preserved_ref IS NOT NULL",
        params![attempt.id.to_string(), now_text()],
    )?;
    if cleared == 0 {
        return Ok(Vec::new());
    }
    tx.execute(
        "UPDATE topology_executions SET status='running',blocked_reason_json=NULL,\
            row_version=row_version+1,updated_at=?2 WHERE id=?1 AND status='blocked'",
        params![execution_id.to_string(), now_text()],
    )?;
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(&attempt.node_id),
            attempt_id: Some(attempt.id),
            actor_kind: "operator",
            ..EventSpec::execution("preserved_work_discard_completed")
        },
    )?;
    tx.commit()?;
    Ok(vec![update])
}

#[cfg(test)]
pub(crate) fn current_row_version(store: &Store, execution_id: Uuid) -> Result<i64> {
    Ok(store.conn.query_row(
        "SELECT row_version FROM topology_executions WHERE id=?1",
        [execution_id.to_string()],
        |row| row.get(0),
    )?)
}

/// Durable TUI projection (plan §6): the snapshot replays `topology_events`.
pub(crate) fn execution_snapshot(
    store: &Store,
    execution_id: Uuid,
) -> Result<Option<WorkflowExecutionSnapshot>> {
    let Some(execution) = load_execution(store, execution_id)? else {
        return Ok(None);
    };
    let mut statement = store.conn.prepare(
        "SELECT payload_json FROM (SELECT payload_json,execution_seq FROM topology_events \
            WHERE execution_id=?1 ORDER BY execution_seq DESC LIMIT ?2) ORDER BY execution_seq",
    )?;
    let updates = statement
        .query_map(
            params![execution_id.to_string(), SNAPSHOT_UPDATE_LIMIT],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|payload| {
            let payload: Value = serde_json::from_str(&payload)?;
            serde_json::from_value::<GraphExecutionUpdate>(payload["update"].clone())
                .map_err(DaemonError::from)
        })
        .collect::<Result<Vec<_>>>()?;
    let blocked_attempt_id = execution
        .blocked_reason
        .as_ref()
        .and_then(|reason| reason.get("attempt_id"))
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok());
    let blocked_reason = execution
        .blocked_reason
        .as_ref()
        .and_then(|reason| reason.get("kind"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Some(WorkflowExecutionSnapshot {
        execution_id,
        workflow_id: execution.workflow_id,
        workflow_name: execution.name,
        status: execution.status.wire(),
        accepted_at: execution.created_at,
        started_at: execution.started_at,
        finished_at: execution.finished_at,
        dry_run: false,
        input: execution.input,
        output: execution.output,
        error: execution.error,
        last_sequence: updates.last().map_or(0, |update| update.sequence),
        updates,
        row_version: Some(execution.row_version),
        blocked_attempt_id,
        blocked_reason,
    }))
}

/// Settlement cleanup still owed by a terminal execution (plan §4 rules 3
/// and 5). Completion is recorded as `pins_released` / `sessions_released`
/// events, so a crash between settling and cleaning is replayed by recovery.
pub(crate) struct CleanupDue {
    pub(crate) execution_id: Uuid,
    pub(crate) repo_root: PathBuf,
    pub(crate) pins: bool,
    pub(crate) sessions: bool,
}

/// Success releases pins and archives node sessions at once; cancellation
/// archives sessions at once; failure preserves both for forensics until the
/// TTL `cutoff` (and cancellation's pins likewise).
pub(crate) fn cleanup_due(
    store: &Store,
    cutoff: DateTime<Utc>,
    limit: usize,
) -> Result<Vec<CleanupDue>> {
    let mut statement = store.conn.prepare(
        "SELECT id,repo_root,pins_due,sessions_due FROM (SELECT e.id,e.repo_root,\
            (NOT EXISTS (SELECT 1 FROM topology_events v WHERE v.execution_id=e.id AND v.kind='pins_released') \
                AND (e.status='succeeded' OR e.finished_at < ?1)) AS pins_due,\
            (NOT EXISTS (SELECT 1 FROM topology_events v WHERE v.execution_id=e.id AND v.kind='sessions_released') \
                AND (e.status IN ('succeeded','cancelled') OR e.finished_at < ?1)) AS sessions_due \
         FROM topology_executions e WHERE e.status IN ('succeeded','failed','cancelled')) \
         WHERE pins_due OR sessions_due LIMIT ?2",
    )?;
    let rows = statement
        .query_map(
            params![
                cutoff.to_rfc3339_opts(SecondsFormat::Nanos, true),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(id, repo, pins, sessions)| {
            Ok(CleanupDue {
                execution_id: parse_uuid(&id)?,
                repo_root: PathBuf::from(repo),
                pins,
                sessions,
            })
        })
        .collect()
}

/// Node sessions a settlement may archive: launched attempts only, never
/// one whose sandbox carries preserved work (only `discard` deletes that).
pub(crate) fn releasable_sessions(store: &Store, execution_id: Uuid) -> Result<Vec<Uuid>> {
    let mut statement = store.conn.prepare(
        "SELECT session_id FROM topology_node_attempts WHERE execution_id=?1 \
         AND started_at IS NOT NULL AND preserved_ref IS NULL AND session_id IS NOT NULL",
    )?;
    let ids = statement
        .query_map([execution_id.to_string()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ids.iter().map(|id| parse_uuid(id)).collect()
}

/// Record one completed settlement cleanup step (idempotent by kind).
pub(crate) fn record_cleanup(
    store: &Store,
    execution_id: Uuid,
    kind: &str,
    count: usize,
) -> Result<Option<GraphExecutionUpdate>> {
    let tx = immediate(store)?;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM topology_events WHERE execution_id=?1 AND kind=?2)",
        params![execution_id.to_string(), kind],
        |row| row.get(0),
    )?;
    let update = if exists {
        None
    } else {
        Some(append_event(
            &tx,
            execution_id,
            EventSpec {
                detail: serde_json::json!({ "count": count }),
                ..EventSpec::execution(kind)
            },
        )?)
    };
    tx.commit()?;
    Ok(update)
}

/// Pins still recorded for one execution.
pub(crate) fn pins_of_execution(
    store: &Store,
    execution_id: Uuid,
) -> Result<Vec<(Uuid, String, String)>> {
    let mut statement = store.conn.prepare(
        "SELECT id,pin_ref,result_commit FROM topology_node_attempts \
         WHERE execution_id=?1 AND pin_ref IS NOT NULL AND result_commit IS NOT NULL",
    )?;
    let rows = statement
        .query_map([execution_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(id, pin, commit)| Ok((parse_uuid(&id)?, pin, commit)))
        .collect()
}

pub(crate) fn clear_pin(store: &Store, attempt_id: Uuid) -> Result<()> {
    store.conn.execute(
        "UPDATE topology_node_attempts SET pin_ref=NULL,updated_at=?2 WHERE id=?1",
        params![attempt_id.to_string(), now_text()],
    )?;
    Ok(())
}

/// Whether a model invocation was admitted for this dedup key.
pub(crate) fn invocation_admitted(store: &Store, dedup_key: &str) -> Result<bool> {
    Ok(crate::model_control::lookup_existing_invocation_by_dedup(store, dedup_key)?.is_some())
}
