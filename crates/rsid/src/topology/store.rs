//! Durable rows for the topology executor (#634, plan §2.1–§2.2).
//!
//! Every mutation runs in one IMMEDIATE transaction that also bumps the
//! execution `row_version` and appends a `topology_events` row. The event
//! payload carries the exact `GraphExecutionUpdate` published on the bus, so
//! the TUI snapshot replays from durable rows after any restart.

use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::types::{
    GraphExecutionUpdate, TopologyOnCallSeat, TopologyOnCallWait, WorkflowExecutionSnapshot,
    WorkflowExecutionStatus, WorkflowNodeExecutionState,
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

/// The text a topology definition is stored as: keys sorted recursively (node
/// `params` is a `HashMap`, so the struct's own serialization order varies per
/// call). The one text every topology digest is taken over (#2693).
pub(crate) fn canonical_definition_text(
    definition: &rsi_common::types::TopologyDefinition,
) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::to_value(definition)?)?)
}

/// The one digest of a topology definition, however its object keys were
/// ordered when the text was written: `validate_only`, the stored
/// `definition_digest`, `expected_digest`, `starter_drift.shipped_digest` and
/// the `SHIPPED_BEFORE` pins all use it (#2693). Text this build cannot read as
/// a definition falls back to the digest of the text itself.
pub(crate) fn definition_digest(text: &str) -> String {
    canonical_digest(text).unwrap_or_else(|| digest(text))
}

/// [`definition_digest`], `None` when the text is not a definition.
pub(crate) fn canonical_digest(text: &str) -> Option<String> {
    let definition: rsi_common::types::TopologyDefinition = serde_json::from_str(text).ok()?;
    Some(digest(&canonical_definition_text(&definition).ok()?))
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
    /// A review node whose assignment is requested and whose verdict is
    /// pending (#1641). No session of the topology's own runs.
    Waiting,
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
            Self::Waiting => "waiting",
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
            "running" => Self::Running,
            "waiting" => Self::Waiting,
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
        matches!(
            self,
            Self::Reserved | Self::Launching | Self::Running | Self::Waiting
        )
    }

    const fn node_state(self) -> WorkflowNodeExecutionState {
        match self {
            Self::Reserved | Self::Launching | Self::Running | Self::Waiting => {
                WorkflowNodeExecutionState::Running
            }
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
    pub(crate) use crate::store_support::topology_usage::{
        LAUNCH_REFUSED, LOST_BEFORE_SESSION, POLICY_REFUSED,
    };
    pub(crate) const SESSION_FAILED: &str = "session_failed";
    pub(crate) const TIMEOUT: &str = "timeout";
    pub(crate) const INTERRUPTED: &str = "interrupted";
    pub(crate) const LOST_AFTER_SESSION: &str = "lost_after_session";
    pub(crate) const PRESERVED_WORK: &str = "preserved_work";
    pub(crate) const CUSTODY_REFUSED: &str = "custody_refused";
    pub(crate) const CANCELLED: &str = "cancelled";
    pub(crate) const DISCARDED: &str = "preserved_work_discarded";
    pub(crate) const HANDOFF_INVALID: &str = "handoff_invalid";
    pub(crate) const HANDOFF_BLOCKED: &str = "handoff_blocked";
    /// A catalog op changed its sandbox (plan §3.2); preserved, never re-run.
    pub(crate) const SANDBOX_MUTATED: &str = "sandbox_mutated";
    pub(crate) const EXIT_NONZERO: &str = "exit_nonzero";
    pub(crate) const GATE_ERROR: &str = "gate_error";
    /// The review service could not give a usable verdict; one new
    /// assignment is tried, then the node blocks.
    pub(crate) const REVIEW_UNSETTLED: &str = "review_unsettled";
    /// #3640: the reviewer's allocation launch was refused or ended without a
    /// session (the action ended `manager_v2_lifecycle_unconfirmed`, or no
    /// reviewer session ever existed): a launch problem, not a review one. Same
    /// retry and resolve rules as [`REVIEW_UNSETTLED`]; the recorded error keeps
    /// the launch's own reason and detail.
    pub(crate) const REVIEW_LAUNCH_REFUSED: &str = "review_launch_refused";

    /// Whether `class` is a review that gave no usable verdict (`review_unsettled`
    /// or its launch-shaped form, [`REVIEW_LAUNCH_REFUSED`]).
    pub(crate) fn is_review_unsettled(class: Option<&str>) -> bool {
        matches!(class, Some(REVIEW_UNSETTLED | REVIEW_LAUNCH_REFUSED))
    }
    /// The reviewer reported that a check it needed could not run (no build
    /// slot, a missing runner): no verdict on the code (#2570). An
    /// infrastructure loss, retried with a fresh review and never charged
    /// against the execution's attempt cap or the review's `max_rounds`.
    pub(crate) const REVIEW_INFRASTRUCTURE: &str = "review_infrastructure";
    /// The review node reached `max_rounds` (or its loop bound) with changes
    /// still requested.
    pub(crate) const REVIEW_ROUNDS_EXHAUSTED: &str = "review_rounds_exhausted";
    /// The on-call manager ruled "one more round" on an exhausted review: the
    /// attempt ends and the instance earns one new review assignment (#1641
    /// S3c). Bounded by `retry_due` to one per instance.
    pub(crate) const REVIEW_REOPENED: &str = "review_reopened";
    /// The project has no live manager ledger to own the review (#1641 S1b).
    /// Retrying the same request cannot help, so it blocks at once.
    pub(crate) const REVIEW_NO_MANAGER_LEDGER: &str = "review_no_manager_ledger";
    /// The accepted review no longer admits the commit (the manager scope
    /// moved, the receipt changed, the requester lost authority): nothing is
    /// enqueued (#1641 S2).
    pub(crate) const LAND_ADMISSION_LOST: &str = "land_admission_lost";
    /// The merge queue refused or failed the entry, or the source could not be
    /// enqueued. Never retried: a second landing attempt is a decision.
    pub(crate) const LAND_REFUSED: &str = "land_refused";
    /// The operator disabled the merge queue; visible, nothing was enqueued.
    pub(crate) const QUEUE_DISABLED: &str = "queue_disabled";
    /// A land admitted by a verified session found a migration in its source:
    /// AGENTS.md reserves pre-land review for those, so nothing is enqueued
    /// (#2249). Relaunch through a topology with a review node.
    pub(crate) const LAND_REQUIRES_REVIEW: &str = "land_requires_review";
    /// A `require_verified` session finished its handoff, but the daemon found
    /// no terminal `scoped_test` job of its own whose receipt verifies the
    /// result commit (#2249). Routable by a `failure` edge; never retried.
    pub(crate) const VERIFICATION_FAILED: &str = "verification_failed";
    /// Every incoming edge of the node was untaken.
    pub(crate) const DEAD_PATH: &str = "dead_path";
    /// The node's session filled its context, committed and passed the #1254
    /// baton (#3340). Not the node's fault: the executor preserves the commit
    /// and relaunches the node from it with the baton as context, bounded per
    /// instance by `baton::MAX_CONTEXT_CAP_CONTINUATIONS`.
    pub(crate) const CONTEXT_CAP_CONTINUED: &str = "context_cap_continued";

    /// Session and review losses that are not the node's fault: bounded per
    /// instance (`MAX_ATTEMPTS_PER_NODE`), never charged against the
    /// execution's attempt cap.
    pub(crate) const LOSSES: [&str; 4] = [
        LOST_BEFORE_SESSION,
        LOST_AFTER_SESSION,
        INTERRUPTED,
        REVIEW_INFRASTRUCTURE,
    ];

    /// Every class that is never charged against the execution's attempt cap:
    /// the losses plus a context-cap continuation (own budget, #3340).
    pub(crate) const INFRASTRUCTURE: [&str; 5] = [
        LOSSES[0],
        LOSSES[1],
        LOSSES[2],
        LOSSES[3],
        CONTEXT_CAP_CONTINUED,
    ];

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
    /// `operator`, `manager`, `epic_lead` or `schedule` (#633).
    pub(crate) requested_by_kind: String,
    #[allow(
        dead_code,
        reason = "requester audit identity; read by the T4b fleet projection"
    )]
    pub(crate) requested_by_session_id: Option<Uuid>,
    pub(crate) epic_id: Option<Uuid>,
    #[allow(
        dead_code,
        reason = "execution provenance; read by the T4b fleet projection"
    )]
    pub(crate) topology_id: Option<Uuid>,
    #[allow(
        dead_code,
        reason = "execution provenance; read by the T4b fleet projection"
    )]
    pub(crate) topology_revision: Option<i64>,
    pub(crate) scope_version: Option<i64>,
}

impl ExecutionRow {
    /// Requested through a scoped agent verb (manager or Epic lead).
    pub(crate) fn agent_requested(&self) -> bool {
        matches!(self.requested_by_kind.as_str(), "manager" | "epic_lead")
    }
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
    /// `session`, `command`, `review`, `land` or `gate`.
    pub(crate) node_kind: String,
    /// Command nodes: sandbox HEAD recorded before the op ran.
    pub(crate) pre_head: Option<String>,
    /// Command nodes: live process group of the op.
    pub(crate) process_group_id: Option<i32>,
    /// Review nodes: the requested review assignment.
    pub(crate) review_assignment_id: Option<Uuid>,
    /// Land nodes: the merge-queue entry this attempt enqueued (stored in
    /// `integrate_action_id`); a restart mirrors it and never re-submits.
    pub(crate) land_entry_id: Option<Uuid>,
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
    /// Scoped agent requester (#633); `None` is the operator.
    pub(crate) requester: Option<ExecutionRequester>,
    /// An operator execution's owning project manager (#1746), resolved by the
    /// daemon from the Epic's project. Always `None` with a `requester`.
    pub(crate) owner: Option<OperatorOwner>,
}

/// The Epic and project manager that own an operator-started execution's
/// review and land nodes. Never taken from a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OperatorOwner {
    pub(crate) epic_id: Uuid,
    pub(crate) manager_session_id: Uuid,
}

/// The token-resolved agent that accepted an execution (#633, plan §5).
#[derive(Clone, Debug)]
pub(crate) struct ExecutionRequester {
    pub(crate) actor: Actor,
    pub(crate) epic_id: Uuid,
    pub(crate) scope_version: Option<i64>,
    pub(crate) policy_digest: Option<String>,
    pub(crate) idempotency_key: String,
    pub(crate) request_fingerprint: String,
    pub(crate) topology_revision: Option<i64>,
}

/// Who a durable topology write is attributed to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Actor {
    /// `operator`, `manager` or `epic_lead`.
    pub(crate) kind: &'static str,
    pub(crate) session_id: Option<Uuid>,
}

impl Actor {
    pub(crate) const OPERATOR: Self = Self {
        kind: "operator",
        session_id: None,
    };

    pub(crate) const fn manager(session_id: Uuid) -> Self {
        Self {
            kind: "manager",
            session_id: Some(session_id),
        }
    }

    pub(crate) const fn epic_lead(session_id: Uuid) -> Self {
        Self {
            kind: "epic_lead",
            session_id: Some(session_id),
        }
    }
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
    pub(crate) actor_session_id: Option<Uuid>,
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
            actor_session_id: None,
        }
    }

    /// Attribute the event to an operator or a token-resolved agent.
    pub(crate) const fn by(mut self, actor: Actor) -> Self {
        self.actor_kind = actor.kind;
        self.actor_session_id = actor.session_id;
        self
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
         VALUES (?1,?2,?3,?4,?5,?6,?7,?10,?8,?9)",
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
            spec.actor_session_id.map(|id| id.to_string()),
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
    let tx = immediate(store)?;
    let update = insert_in(&tx, new)?;
    tx.commit()?;
    Ok(update)
}

/// Outcome of accepting an agent-requested execution (#633, plan §5.1).
pub(crate) enum Acceptance {
    Fresh(GraphExecutionUpdate),
    /// The same caller already accepted this idempotency key with the same
    /// request fingerprint; nothing was written.
    Replay {
        execution_id: Uuid,
        accepted_at: DateTime<Utc>,
        base_commit: String,
    },
}

/// A prior acceptance of `(requested_by_session_id, idempotency_key)`.
pub(crate) struct PriorAcceptance {
    pub(crate) execution_id: Uuid,
    pub(crate) fingerprint: Option<String>,
    pub(crate) accepted_at: DateTime<Utc>,
    pub(crate) base_commit: String,
}

pub(crate) fn prior_acceptance(
    conn: &Connection,
    requester: Uuid,
    idempotency_key: &str,
) -> Result<Option<PriorAcceptance>> {
    let row: Option<(String, Option<String>, String, String)> = conn
        .query_row(
            "SELECT id,request_fingerprint,created_at,base_commit FROM topology_executions \
             WHERE requested_by_session_id=?1 AND idempotency_key=?2",
            params![requester.to_string(), idempotency_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    row.map(|(id, fingerprint, created_at, base_commit)| {
        Ok(PriorAcceptance {
            execution_id: parse_uuid(&id)?,
            fingerprint,
            accepted_at: parse_time(&created_at)?,
            base_commit,
        })
    })
    .transpose()
}

/// Accept one agent execution atomically with its idempotency check: the
/// same key and fingerprint replay, a different fingerprint conflicts.
pub(crate) fn accept_agent_execution(store: &Store, new: &NewExecution) -> Result<Acceptance> {
    let requester = new
        .requester
        .as_ref()
        .ok_or_else(|| DaemonError::Store("agent execution without requester".into()))?;
    let caller = requester
        .actor
        .session_id
        .ok_or_else(|| DaemonError::Store("agent execution without caller".into()))?;
    let tx = immediate(store)?;
    if let Some(prior) = prior_acceptance(&tx, caller, &requester.idempotency_key)? {
        if prior.fingerprint.as_deref() != Some(requester.request_fingerprint.as_str()) {
            return Err(super::resolve::resolution_error(
                "idempotency_conflict",
                "retry the original request or use a new idempotency_key",
                None,
                None,
            ));
        }
        return Ok(Acceptance::Replay {
            execution_id: prior.execution_id,
            accepted_at: prior.accepted_at,
            base_commit: prior.base_commit,
        });
    }
    let update = insert_in(&tx, new)?;
    tx.commit()?;
    Ok(Acceptance::Fresh(update))
}

fn insert_in(tx: &Connection, new: &NewExecution) -> Result<GraphExecutionUpdate> {
    let definition = serde_json::to_string(&new.definition)?;
    let custody_plan = serde_json::to_string(&new.custody_plan)?;
    let now = now_text();
    let requester = new.requester.as_ref();
    let actor = requester.map_or(Actor::OPERATOR, |requester| requester.actor);
    tx.execute(
        "INSERT INTO topology_executions (id,topology_id,topology_revision,topology_name_snapshot,workflow_id,definition_json,definition_digest,project_id,parent_session_id,requested_by_kind,repo_root,base_ref,base_commit,custody_plan_json,status,input_json,max_node_attempts,created_at,updated_at,epic_id,requested_by_session_id,scope_version,policy_digest,idempotency_key,request_fingerprint) \
         VALUES (?1,(SELECT id FROM topologies WHERE id=?2),?15,?3,?4,?5,?6,?7,?8,?16,?9,'refs/remotes/origin/rolling',?10,?11,'accepted',?12,?13,?14,?14,?17,?18,?19,?20,?21,?22)",
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
            requester.and_then(|requester| requester.topology_revision),
            actor.kind,
            requester
                .map(|requester| requester.epic_id)
                .or(new.owner.map(|owner| owner.epic_id))
                .map(|id| id.to_string()),
            actor
                .session_id
                .or(new.owner.map(|owner| owner.manager_session_id))
                .map(|id| id.to_string()),
            requester.and_then(|requester| requester.scope_version),
            requester.and_then(|requester| requester.policy_digest.clone()),
            requester.map(|requester| requester.idempotency_key.clone()),
            requester.map(|requester| requester.request_fingerprint.clone()),
        ],
    )?;
    append_event(tx, new.id, EventSpec::execution("accepted").by(actor))
}

const EXECUTION_COLUMNS: &str = "id,topology_id,workflow_id,topology_name_snapshot,definition_json,custody_plan_json,project_id,parent_session_id,repo_root,base_commit,status,blocked_reason_json,input_json,output_json,error,row_version,max_node_attempts,deadline_at,created_at,started_at,finished_at,requested_by_kind,requested_by_session_id,epic_id,topology_revision,scope_version";

fn execution_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Vec<Option<String>>> {
    (0..26)
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
    let optional_i64 = |index: usize| {
        c[index]
            .as_deref()
            .map(|text| {
                text.parse::<i64>()
                    .map_err(|_| DaemonError::Store("invalid topology integer".into()))
            })
            .transpose()
    };
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
        requested_by_kind: required(&c, 21)?.to_owned(),
        requested_by_session_id: optional_uuid(22)?,
        epic_id: optional_uuid(23)?,
        topology_id: optional_uuid(1)?,
        topology_revision: optional_i64(24)?,
        scope_version: optional_i64(25)?,
    }))
}

const ATTEMPT_COLUMNS: &str = "id,node_id,iteration,attempt_no,status,dedup_key,session_id,boot_id,sandbox_root,base_commit,result_commit,pin_ref,input_json,output_json,failure_class,error,resolution,preserved_ref,preserved_commit,started_at,node_kind,pre_head,process_group_id,review_assignment_id,integrate_action_id";

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
        review_assignment_id: text(23)?.as_deref().map(parse_uuid).transpose()?,
        land_entry_id: text(24)?.as_deref().map(parse_uuid).transpose()?,
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

/// When an attempt settled (`finished_at`), for the backoff before an
/// automatic retry (#2280).
pub(crate) fn attempt_finished_at(
    store: &Store,
    attempt_id: Uuid,
) -> Result<Option<DateTime<Utc>>> {
    let finished: Option<Option<String>> = store
        .conn
        .query_row(
            "SELECT finished_at FROM topology_node_attempts WHERE id=?1",
            [attempt_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    finished.flatten().as_deref().map(parse_time).transpose()
}

pub(crate) fn load_attempt(store: &Store, attempt_id: Uuid) -> Result<Option<(Uuid, AttemptRow)>> {
    let mut statement = store.conn.prepare(&format!(
        "SELECT {ATTEMPT_COLUMNS},execution_id FROM topology_node_attempts WHERE id=?1"
    ))?;
    let mut rows = statement.query([attempt_id.to_string()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let execution_id = parse_uuid(&row.get::<_, String>(25)?)?;
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
            WHERE execution_id=?1 AND (failure_class IS NULL OR failure_class NOT IN (?2,?3,?4,?5,?6))) \
         FROM topology_executions WHERE id=?1",
        params![
            execution_id.to_string(),
            failure::INFRASTRUCTURE[0],
            failure::INFRASTRUCTURE[1],
            failure::INFRASTRUCTURE[2],
            failure::INFRASTRUCTURE[3],
            failure::INFRASTRUCTURE[4],
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

/// #1641 S4b: `launching` → `reserved` for an attempt whose launch was held
/// before any session, sandbox or invocation existed. Only a launching row
/// with no recorded sandbox moves back, so a command that may have started is
/// never made launchable twice.
pub(crate) fn release_launching(store: &Store, attempt_id: Uuid) -> Result<bool> {
    let changed = store.conn.execute(
        "UPDATE topology_node_attempts SET status='reserved',boot_id=NULL,updated_at=?2 \
         WHERE id=?1 AND status='launching' AND sandbox_root IS NULL",
        params![attempt_id.to_string(), now_text()],
    )?;
    Ok(changed == 1)
}

/// The `kind` of the attempt's latest `admission_hold` event when no other
/// event for the attempt followed it (the attempt is still held for it).
fn standing_admission_hold(conn: &Connection, attempt_id: Uuid) -> Result<Option<String>> {
    let latest: Option<(String, String)> = conn
        .query_row(
            "SELECT kind,payload_json FROM topology_events WHERE attempt_id=?1 \
             ORDER BY execution_seq DESC LIMIT 1",
            [attempt_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(latest.and_then(|(kind, payload)| {
        (kind == "admission_hold")
            .then(|| serde_json::from_str::<Value>(&payload).ok())
            .flatten()
            .and_then(|payload| payload["detail"]["kind"].as_str().map(str::to_owned))
    }))
}

/// #1641 S4b: record that resource pressure holds the attempt `reserved`.
/// One `admission_hold{kind}` event per transition: a tick that finds the
/// attempt still held for the same kind writes nothing (`None`).
pub(crate) fn note_admission_hold(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    kind: &str,
) -> Result<Option<GraphExecutionUpdate>> {
    let tx = immediate(store)?;
    if standing_admission_hold(&tx, attempt.id)?.as_deref() == Some(kind) {
        return Ok(None);
    }
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(&attempt.node_id),
            attempt_id: Some(attempt.id),
            detail: serde_json::json!({
                "kind": kind,
                "iteration": attempt.iteration,
                "attempt_no": attempt.attempt_no,
            }),
            ..EventSpec::execution("admission_hold")
        },
    )?;
    tx.commit()?;
    Ok(Some(update))
}

/// #2260: record what a published land did to its input Issue: one
/// `topology_landed` event when this execution closed it, one
/// `issue_close_refused` when the owner could not. At most one event of a
/// `kind` per attempt (`None` when it already exists), so the close re-run
/// after a crash between the Issue write and the land's settlement writes no
/// second note.
pub(crate) fn note_land_issue(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    owner: Option<Uuid>,
    kind: &str,
    detail: Value,
) -> Result<Option<GraphExecutionUpdate>> {
    let tx = immediate(store)?;
    let noted: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM topology_events WHERE attempt_id=?1 AND kind=?2)",
        params![attempt.id.to_string(), kind],
        |row| row.get(0),
    )?;
    if noted {
        return Ok(None);
    }
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(&attempt.node_id),
            attempt_id: Some(attempt.id),
            detail,
            actor_session_id: owner,
            ..EventSpec::execution(kind)
        },
    )?;
    tx.commit()?;
    Ok(Some(update))
}

/// #3036: the event a land attempt refused `queue_batch_merge_conflict`
/// records when it waits for conflicting queue entries ahead of it.
pub(crate) const LAND_CONFLICT_WAIT: &str = "land_conflict_wait";

/// #3036: record that the land attempt waits for queue entries `waiting_for`
/// to settle before it settles its refusal. At most one per attempt (written
/// through [`note_land_issue`]'s once-per-kind rule), so the scan that chose
/// them runs once and a restart reads the same set back.
pub(crate) fn note_land_conflict_wait(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    entry_id: Uuid,
    waiting_for: &[Uuid],
) -> Result<Option<GraphExecutionUpdate>> {
    note_land_issue(
        store,
        execution_id,
        attempt,
        None,
        LAND_CONFLICT_WAIT,
        serde_json::json!({
            "node_id": attempt.node_id,
            "iteration": attempt.iteration,
            "attempt_no": attempt.attempt_no,
            "entry_id": entry_id,
            "waiting_for": waiting_for,
        }),
    )
}

/// #3036: the queue entries the attempt's `land_conflict_wait` event names,
/// or `None` when it recorded no wait.
pub(crate) fn land_conflict_wait(store: &Store, attempt_id: Uuid) -> Result<Option<Vec<Uuid>>> {
    let payload: Option<String> = store
        .conn
        .query_row(
            "SELECT payload_json FROM topology_events WHERE attempt_id=?1 AND kind=?2 \
             ORDER BY execution_seq LIMIT 1",
            params![attempt_id.to_string(), LAND_CONFLICT_WAIT],
            |row| row.get(0),
        )
        .optional()?;
    let Some(payload) = payload else {
        return Ok(None);
    };
    // An unreadable record holds nothing: the refusal settles.
    let payload: Value = serde_json::from_str(&payload).unwrap_or_default();
    let entries = payload["detail"]["waiting_for"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|id| Uuid::parse_str(id).ok())
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(entries))
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

/// Review nodes: `reserved`/`launching` → `waiting`, recording the requested
/// assignment. Idempotent for the same assignment.
pub(crate) fn mark_waiting(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    assignment_id: Uuid,
    boot_id: Uuid,
) -> Result<GraphExecutionUpdate> {
    let tx = immediate(store)?;
    tx.execute(
        "UPDATE topology_node_attempts SET status='waiting',review_assignment_id=?2,boot_id=?3,\
            started_at=COALESCE(started_at,?4),updated_at=?4 \
         WHERE id=?1 AND status IN ('reserved','launching')",
        params![
            attempt.id.to_string(),
            assignment_id.to_string(),
            boot_id.to_string(),
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
                "review_assignment_id": assignment_id,
            }),
            ..EventSpec::execution("node_waiting")
        },
    )?;
    tx.commit()?;
    Ok(update)
}

/// Land nodes: `reserved`/`launching` → `waiting`, recording the merge-queue
/// entry. Idempotent for the same entry.
pub(crate) fn mark_land_waiting(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    entry_id: Uuid,
    boot_id: Uuid,
) -> Result<GraphExecutionUpdate> {
    let tx = immediate(store)?;
    tx.execute(
        "UPDATE topology_node_attempts SET status='waiting',integrate_action_id=?2,boot_id=?3,\
            started_at=COALESCE(started_at,?4),updated_at=?4 \
         WHERE id=?1 AND status IN ('reserved','launching')",
        params![
            attempt.id.to_string(),
            entry_id.to_string(),
            boot_id.to_string(),
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
                "land_entry_id": entry_id,
            }),
            ..EventSpec::execution("node_waiting")
        },
    )?;
    tx.commit()?;
    Ok(update)
}

/// #3358: a waiting land attempt follows its queue entry's in-place successor:
/// the recorded entry becomes `successor` (same position, same wake) and one
/// `land_entry_followed` event names both. `None` when the attempt no longer
/// waits on `entry` (a concurrent tick followed it already).
pub(crate) fn follow_land_entry(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    entry: Uuid,
    successor: Uuid,
) -> Result<Option<GraphExecutionUpdate>> {
    let tx = immediate(store)?;
    let changed = tx.execute(
        "UPDATE topology_node_attempts SET integrate_action_id=?3,updated_at=?4 \
         WHERE id=?1 AND status='waiting' AND integrate_action_id=?2",
        params![
            attempt.id.to_string(),
            entry.to_string(),
            successor.to_string(),
            now_text(),
        ],
    )?;
    if changed != 1 {
        return Ok(None);
    }
    bump(&tx, execution_id)?;
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(&attempt.node_id),
            attempt_id: Some(attempt.id),
            detail: serde_json::json!({
                "iteration": attempt.iteration,
                "attempt_no": attempt.attempt_no,
                "superseded_entry_id": entry,
                "land_entry_id": successor,
            }),
            ..EventSpec::execution("land_entry_followed")
        },
    )?;
    tx.commit()?;
    Ok(Some(update))
}

/// #1641 S3c: park an attempt on a decision record. A session attempt goes
/// `running` → `waiting` (its session has ended with a question); a review
/// attempt is already `waiting` and only gains the decision marker. `output`
/// carries `decision_key` and whatever the settlement needs later; it is
/// replaced by the real output when the attempt settles. The node's wait on
/// the ruling opens in the same transaction (`wait_floor`: the last moment the
/// node is known not to have waited).
pub(crate) fn mark_decision_waiting(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    output: &Value,
    wait_floor: DateTime<Utc>,
) -> Result<Vec<GraphExecutionUpdate>> {
    let tx = immediate(store)?;
    let changed = tx.execute(
        "UPDATE topology_node_attempts SET status='waiting',output_json=?2,updated_at=?3 \
         WHERE id=?1 AND status IN ('running','waiting')",
        params![attempt.id.to_string(), output.to_string(), now_text()],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "topology attempt cannot wait on a decision".into(),
        ));
    }
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
                "decision_key": output.get("decision_key"),
                "decision_kind": output.get("decision_kind"),
            }),
            ..EventSpec::execution("node_waiting_on_decision")
        },
    )?;
    // The ruling wait starts with the parking transition, so a daemon that
    // stops right after it and returns hours later still charges that time to
    // the wait, not to the node's productive clock (#1715).
    let opened = open_wait_in(
        &tx,
        execution_id,
        &attempt.node_id,
        attempt.id,
        Some(Utc::now()),
        wait_floor,
    )?;
    tx.commit()?;
    Ok(std::iter::once(update).chain(opened).collect())
}

/// Rewrite the marker of an attempt that is already parked on a decision (a
/// delivery failure counter); no event.
pub(crate) fn update_decision_marker(
    store: &Store,
    attempt_id: Uuid,
    output: &Value,
) -> Result<()> {
    store.conn.execute(
        "UPDATE topology_node_attempts SET output_json=?2,updated_at=?3 \
         WHERE id=?1 AND status='waiting'",
        params![attempt_id.to_string(), output.to_string(), now_text()],
    )?;
    Ok(())
}

/// Typed refusal: the attempt or execution an answer was bound to is no longer
/// live (cancelled, past its deadline, settled or moved on) (#1715).
pub(crate) const ANSWER_REVOKED: &str = "topology_answer_revoked";
/// Typed refusal: an earlier delivery of this answer may have reached the
/// provider. It is shown as uncertain and never replayed (#945).
pub(crate) const ANSWER_UNCERTAIN: &str = "topology_answer_delivery_uncertain";

/// The exact topology binding of one answer continuation: the execution, the
/// attempt that waits for it and the decision it answers (#1715).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AnswerBinding {
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) decision_key: String,
}

/// The marker of the bound attempt when the binding is still exactly live:
/// the attempt waits on this decision in this session, and its execution is
/// running (not cancelling or settled) and inside its absolute deadline.
fn answer_target(conn: &Connection, binding: &AnswerBinding, session_id: Uuid) -> Result<Value> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        String,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
    )> = conn
        .query_row(
            "SELECT a.status,a.session_id,a.node_kind,a.output_json,e.status,e.deadline_at \
             FROM topology_node_attempts a JOIN topology_executions e ON e.id=a.execution_id \
             WHERE a.id=?1 AND a.execution_id=?2",
            params![
                binding.attempt_id.to_string(),
                binding.execution_id.to_string()
            ],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let revoked = |why: &str| DaemonError::PolicyDenied(format!("{ANSWER_REVOKED}: {why}"));
    let Some((status, session, kind, output, execution_status, deadline)) = row else {
        return Err(revoked("the attempt is gone"));
    };
    if status != "waiting" || kind != "session" || session != session_id.to_string() {
        return Err(revoked(
            "the attempt no longer waits for this session's answer",
        ));
    }
    if !matches!(
        execution_status.as_str(),
        "accepted" | "running" | "blocked"
    ) {
        return Err(revoked(&format!("the execution is {execution_status}")));
    }
    if let Some(deadline) = deadline
        && parse_time(&deadline)? <= Utc::now()
    {
        return Err(revoked("the execution deadline passed"));
    }
    let marker: Value = match output {
        Some(text) => serde_json::from_str(&text)?,
        None => Value::Null,
    };
    if marker["decision_key"].as_str() != Some(binding.decision_key.as_str()) {
        return Err(revoked("the attempt waits on another decision"));
    }
    Ok(marker)
}

/// Whether the answer binding is still exactly live; no write. Checked under
/// the continuation's spawn guard before any effect (#1715).
pub(crate) fn answer_delivery_live(
    store: &Store,
    binding: &AnswerBinding,
    session_id: Uuid,
) -> Result<()> {
    answer_target(&store.conn, binding, session_id).map(|_| ())
}

/// Revalidate the binding and record, in one transaction, that the answer's
/// provider effect is about to start (`delivery.state = "started"`). This is
/// the durable at-most-once claim: a second claim, or a recovery that finds
/// it, never sends the answer again (#1715).
pub(crate) fn claim_answer_delivery(
    store: &Store,
    binding: &AnswerBinding,
    session_id: Uuid,
) -> Result<()> {
    let tx = immediate(store)?;
    let mut marker = answer_target(&tx, binding, session_id)?;
    if marker["delivery"]["state"] == DELIVERY_STARTED {
        return Err(DaemonError::PolicyDenied(format!(
            "{ANSWER_UNCERTAIN}: an earlier delivery of this answer may have reached the provider"
        )));
    }
    marker["delivery"] = serde_json::json!({ "state": DELIVERY_STARTED, "at": now_text() });
    tx.execute(
        "UPDATE topology_node_attempts SET output_json=?2,updated_at=?3 \
         WHERE id=?1 AND status='waiting'",
        params![
            binding.attempt_id.to_string(),
            marker.to_string(),
            now_text()
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// The provider process was never started (the spawn itself failed): take the
/// claim back so the delivery can be retried. A claim whose effect may have
/// started is never released.
pub(crate) fn release_answer_delivery(store: &Store, binding: &AnswerBinding) -> Result<()> {
    let tx = immediate(store)?;
    let output: Option<Option<String>> = tx
        .query_row(
            "SELECT output_json FROM topology_node_attempts WHERE id=?1 AND status='waiting'",
            [binding.attempt_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(Some(text)) = output {
        let mut marker: Value = serde_json::from_str(&text)?;
        if marker["delivery"]["state"] == DELIVERY_STARTED
            && let Some(map) = marker.as_object_mut()
        {
            map.remove("delivery");
            tx.execute(
                "UPDATE topology_node_attempts SET output_json=?2,updated_at=?3 WHERE id=?1",
                params![
                    binding.attempt_id.to_string(),
                    marker.to_string(),
                    now_text()
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Whether the attempt's answer delivery was claimed (its provider effect may
/// have started).
pub(crate) fn answer_delivery_started(store: &Store, attempt_id: Uuid) -> Result<bool> {
    let output: Option<Option<String>> = store
        .conn
        .query_row(
            "SELECT output_json FROM topology_node_attempts WHERE id=?1",
            [attempt_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match output.flatten() {
        Some(text) => {
            serde_json::from_str::<Value>(&text)?["delivery"]["state"] == DELIVERY_STARTED
        }
        None => false,
    })
}

/// `delivery.state` of a marker whose answer's provider effect may have started.
pub(crate) const DELIVERY_STARTED: &str = "started";

/// #1641 S3c: the ruling arrived and the same session was continued:
/// `waiting` → `running`, the decision marker cleared.
pub(crate) fn resume_after_decision(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    decision_key: &str,
) -> Result<GraphExecutionUpdate> {
    let tx = immediate(store)?;
    let changed = tx.execute(
        "UPDATE topology_node_attempts SET status='running',output_json=NULL,output_digest=NULL,\
            updated_at=?2 WHERE id=?1 AND status='waiting'",
        params![attempt.id.to_string(), now_text()],
    )?;
    if changed != 1 {
        return Err(DaemonError::Store(
            "topology attempt is not waiting on a decision".into(),
        ));
    }
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
                "decision_key": decision_key,
            }),
            ..EventSpec::execution("node_continued_after_decision")
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

/// Checkpoint a session node's last observation. On the first observation in
/// a new boot, exclude the gap since its last checkpoint, retaining all prior
/// runtime. Answer waits already pause the clock, so exclude only the part of
/// the gap outside that wait. One event per boot makes recovery replay safe.
pub(crate) fn checkpoint_node_clock(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    boot_id: Uuid,
    waiting_since: Option<DateTime<Utc>>,
) -> Result<(chrono::TimeDelta, Option<GraphExecutionUpdate>)> {
    const REBASED: &str = "node_clock_rebased";
    let tx = immediate(store)?;
    let (mut excluded_ms, rebased): (i64, bool) = tx.query_row(
        "SELECT COALESCE(SUM(json_extract(payload_json,'$.detail.excluded_ms')),0), \
         COALESCE(MAX(json_extract(payload_json,'$.detail.boot_id')=?3),0) \
         FROM topology_events WHERE attempt_id=?1 AND kind=?2",
        params![attempt.id.to_string(), REBASED, boot_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let now = Utc::now();
    let update = if attempt.boot_id != Some(boot_id) && !rebased {
        let last_seen: String = tx.query_row(
            "SELECT updated_at FROM topology_node_attempts WHERE id=?1",
            [attempt.id.to_string()],
            |row| row.get(0),
        )?;
        let last_seen = parse_time(&last_seen)?
            .max(attempt.started_at.unwrap_or(now))
            .min(now);
        let wait = node_waited_on(&tx, attempt.id)?
            .open_since
            .or(waiting_since)
            .into_iter()
            .chain(build_admission_waited_on(&tx, attempt.id)?.open_since)
            .min();
        let gap_end = wait.map_or(now, |since| since.max(last_seen).min(now));
        let gap_ms = (gap_end - last_seen).num_milliseconds();
        excluded_ms += gap_ms;
        bump(&tx, execution_id)?;
        Some(append_event(
            &tx,
            execution_id,
            EventSpec {
                node_id: Some(&attempt.node_id),
                attempt_id: Some(attempt.id),
                detail: serde_json::json!({ "boot_id": boot_id, "excluded_ms": gap_ms }),
                ..EventSpec::execution(REBASED)
            },
        )?)
    } else {
        None
    };
    tx.execute(
        "UPDATE topology_node_attempts SET updated_at=?2 WHERE id=?1",
        params![
            attempt.id.to_string(),
            now.to_rfc3339_opts(SecondsFormat::Nanos, true)
        ],
    )?;
    tx.commit()?;
    Ok((chrono::TimeDelta::milliseconds(excluded_ms), update))
}

/// The restart journal must not revive workers after their execution settled.
/// Use its existing `not_needed` outcome, both before claim and before dispatch.
pub(crate) fn retire_terminal_node_restart(
    store: &Store,
    intent_id: Uuid,
    session_id: Uuid,
) -> Result<bool> {
    Ok(store.conn.execute(
        "UPDATE daemon_restart_intents SET state='failed',outcome='not_needed',updated_at=?3 \
         WHERE id=?1 AND session_id=?2 AND state IN ('pending','claimed') AND EXISTS ( \
           SELECT 1 FROM topology_node_attempts a JOIN topology_executions e ON e.id=a.execution_id \
           WHERE a.session_id=?2 AND e.status IN ('succeeded','failed','cancelled'))",
        params![intent_id.to_string(), session_id.to_string(), now_text()],
    )? == 1)
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
    // A settled attempt owns no wait on an answer any more (#1715): the stall
    // exemption of its session must not outlive it, whichever path settled it
    // (timeout, cancel, block, success).
    crate::topology::oncall::note_answer_wait(attempt.session_id, false);
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
            review_assignment_id: None,
            land_entry_id: None,
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
    /// A ruled review attempt (#2280): the typed review output the edges read.
    pub(crate) output: Option<Value>,
    /// A ruled review attempt (#2280): the assignment the ruling admits.
    pub(crate) review_assignment: Option<Uuid>,
    pub(crate) retry: Option<NewAttempt>,
    pub(crate) event_kind: &'a str,
    pub(crate) detail: Value,
    /// Request fingerprint bound to the idempotency key.
    pub(crate) fingerprint: &'a str,
    /// Leave `blocked` now; a discard resumes only once its effects finish.
    pub(crate) resume: bool,
    /// Operator or token-resolved agent (#633); recorded on the attempt and
    /// the audit event.
    pub(crate) actor: Actor,
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
    let output = write.output.as_ref().map(Value::to_string);
    if output
        .as_ref()
        .is_some_and(|text| text.len() > OUTPUT_INLINE_LIMIT)
    {
        return Err(DaemonError::Store(
            "topology output exceeds the inline limit".into(),
        ));
    }
    let changed = tx.execute(
        "UPDATE topology_node_attempts SET status=?2,resolution=COALESCE(?3,resolution),failure_class=COALESCE(?4,failure_class),\
            result_commit=COALESCE(?5,result_commit),pin_ref=COALESCE(?6,pin_ref),resolved_by_kind=?8,\
            resolved_by_session_id=?9,resolved_at=?7,updated_at=?7,\
            output_json=COALESCE(?10,output_json),output_digest=COALESCE(?11,output_digest),\
            review_assignment_id=COALESCE(?12,review_assignment_id) WHERE id=?1 AND status='blocked'",
        params![
            write.attempt.id.to_string(),
            write.attempt_status.as_str(),
            write.resolution,
            write.failure_class,
            write.result_commit,
            write.pin_ref,
            now,
            write.actor.kind,
            write.actor.session_id.map(|id| id.to_string()),
            output,
            output.as_deref().map(digest),
            write.review_assignment.map(|id| id.to_string()),
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
            ..EventSpec::execution(write.event_kind).by(write.actor)
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

/// Agent interrupt (#633, plan §5.1): the row-version CAS, the idempotency
/// key (the per-execution namespace resolutions use) and the `cancelling`
/// transition commit in one transaction, audited with the actor.
pub(crate) fn request_agent_interrupt(
    store: &Store,
    execution_id: Uuid,
    expected_row_version: i64,
    idempotency_key: &str,
    actor: Actor,
) -> Result<(Recorded, ExecutionStatus)> {
    let fingerprint = digest(&format!("interrupt|{execution_id}|{expected_row_version}"));
    let tx = immediate(store)?;
    if matches!(
        gate_in(&tx, execution_id, idempotency_key, &fingerprint)?,
        ResolutionGate::Replay
    ) {
        let status = header(&tx, execution_id)?.status;
        return Ok((Recorded::Replay, status));
    }
    let (actual, current): (i64, String) = tx.query_row(
        "SELECT row_version,status FROM topology_executions WHERE id=?1",
        [execution_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if actual != expected_row_version {
        return Err(super::resolve::resolution_error(
            "stale_row_version",
            "refresh the execution and retry with its current row_version",
            Some(expected_row_version),
            Some(actual),
        ));
    }
    let current = ExecutionStatus::parse(&current)?;
    if current.is_final() {
        return Err(super::resolve::resolution_error(
            "precondition_failed",
            "the execution already settled; nothing to interrupt",
            None,
            None,
        ));
    }
    let now = now_text();
    tx.execute(
        "UPDATE topology_executions SET status='cancelling',blocked_reason_json=NULL,\
            started_at=COALESCE(started_at,?2),row_version=row_version+1,updated_at=?2 WHERE id=?1",
        params![execution_id.to_string(), now],
    )?;
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            detail: serde_json::json!({
                "from": current.as_str(),
                "to": ExecutionStatus::Cancelling.as_str(),
                "action": "interrupt",
                "idempotency_key": idempotency_key,
                "fingerprint": fingerprint,
            }),
            ..EventSpec::execution(ExecutionStatus::Cancelling.as_str()).by(actor)
        },
    )?;
    tx.commit()?;
    Ok((Recorded::Fresh(vec![update]), ExecutionStatus::Cancelling))
}

/// Durable audit of one refused agent request against an execution the
/// caller may see (#633, plan §5.3). Not a state change: no row-version bump.
pub(crate) fn record_agent_refusal(
    store: &Store,
    execution_id: Uuid,
    actor: Actor,
    verb: &str,
    code: &str,
) -> Result<GraphExecutionUpdate> {
    let tx = immediate(store)?;
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            detail: serde_json::json!({ "verb": verb, "code": code }),
            ..EventSpec::execution("agent_request_refused").by(actor)
        },
    )?;
    tx.commit()?;
    Ok(update)
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
    actor: Actor,
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
            ..EventSpec::execution("preserved_work_inspected").by(actor)
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

/// Event kinds of the on-call wait (#1641 S3a): one `unavailable` per
/// transition into the wait, one `restored` per transition out of it.
const ON_CALL_UNAVAILABLE: &str = "on_call_unavailable";
const ON_CALL_RESTORED: &str = "on_call_restored";

/// The nodes whose latest on-call event is `on_call_unavailable`, oldest
/// first. Re-derived from the event log, so it survives a restart.
pub(crate) fn on_call_waits(store: &Store, execution_id: Uuid) -> Result<Vec<TopologyOnCallWait>> {
    on_call_waits_on(&store.conn, execution_id)
}

fn on_call_waits_on(conn: &Connection, execution_id: Uuid) -> Result<Vec<TopologyOnCallWait>> {
    let mut statement = conn.prepare(
        "SELECT e.node_id,e.payload_json,e.created_at FROM topology_events e \
         WHERE e.execution_id=?1 AND e.kind=?2 AND e.execution_seq=(\
            SELECT MAX(x.execution_seq) FROM topology_events x \
            WHERE x.execution_id=e.execution_id AND x.node_id=e.node_id AND x.kind IN (?2,?3)) \
         ORDER BY e.execution_seq",
    )?;
    let rows = statement
        .query_map(
            params![
                execution_id.to_string(),
                ON_CALL_UNAVAILABLE,
                ON_CALL_RESTORED
            ],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .filter_map(|(node, payload, at)| node.map(|node| (node, payload, at)))
        .map(|(node_id, payload, at)| {
            let payload: Value = serde_json::from_str(&payload)?;
            let detail = &payload["detail"];
            Ok(TopologyOnCallWait {
                node_id,
                reason: detail["reason"].as_str().unwrap_or("unknown").to_owned(),
                on_call: serde_json::from_value::<TopologyOnCallSeat>(detail["on_call"].clone())
                    .unwrap_or_default(),
                since: parse_time(&at)?,
            })
        })
        .collect()
}

/// Record that `node_id` needs the on-call manager and none is live. Once per
/// transition: `None` when the node is already waiting.
pub(crate) fn record_on_call_unavailable(
    store: &Store,
    execution_id: Uuid,
    node_id: &str,
    attempt_id: Uuid,
    reason: &str,
    seat: &TopologyOnCallSeat,
) -> Result<Option<GraphExecutionUpdate>> {
    let tx = immediate(store)?;
    if on_call_waits_on(&tx, execution_id)?
        .iter()
        .any(|wait| wait.node_id == node_id)
    {
        return Ok(None);
    }
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(node_id),
            attempt_id: Some(attempt_id),
            detail: serde_json::json!({ "reason": reason, "on_call": seat }),
            ..EventSpec::execution(ON_CALL_UNAVAILABLE)
        },
    )?;
    tx.commit()?;
    Ok(Some(update))
}

/// Record that `node_id` no longer waits for the on-call manager (a seat is
/// live again, or the node stopped needing one). `None` when it was not
/// waiting.
pub(crate) fn record_on_call_restored(
    store: &Store,
    execution_id: Uuid,
    node_id: &str,
    why: &str,
) -> Result<Option<GraphExecutionUpdate>> {
    let tx = immediate(store)?;
    if !on_call_waits_on(&tx, execution_id)?
        .iter()
        .any(|wait| wait.node_id == node_id)
    {
        return Ok(None);
    }
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(node_id),
            detail: serde_json::json!({ "why": why }),
            ..EventSpec::execution(ON_CALL_RESTORED)
        },
    )?;
    tx.commit()?;
    Ok(Some(update))
}

/// #1641 S4a: a node session cut off by a restart was continued as the same
/// session (`node_resumed_after_restart`), or its continuation was refused
/// (`node_resume_refused`) and the attempt fell back to preserve/retry. At
/// most one of the two is recorded per attempt per daemon boot, so a boot
/// never resumes an attempt twice.
const NODE_RESUMED_AFTER_RESTART: &str = "node_resumed_after_restart";
const NODE_RESUME_REFUSED: &str = "node_resume_refused";

/// Whether this boot already tried to resume `attempt_id`.
pub(crate) fn node_resume_tried(store: &Store, attempt_id: Uuid, boot_id: Uuid) -> Result<bool> {
    Ok(store.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM topology_events WHERE attempt_id=?1 AND kind IN (?2,?3) \
         AND json_extract(payload_json,'$.detail.boot_id')=?4)",
        params![
            attempt_id.to_string(),
            NODE_RESUMED_AFTER_RESTART,
            NODE_RESUME_REFUSED,
            boot_id.to_string()
        ],
        |row| row.get(0),
    )?)
}

/// Record the outcome of this boot's one resume of `attempt_id`: `Ok(())` was
/// continued (the attempt stays `running`), `Err(reason)` was refused. The
/// boot stamp moves to this boot in the same transaction, so a later failure
/// of the continued session is an ordinary node failure, not a second
/// restart cut.
pub(crate) fn record_node_resume(
    store: &Store,
    execution_id: Uuid,
    node_id: &str,
    attempt_id: Uuid,
    boot_id: Uuid,
    outcome: std::result::Result<(), &str>,
) -> Result<GraphExecutionUpdate> {
    let tx = immediate(store)?;
    let (kind, detail) = match outcome {
        Ok(()) => (
            NODE_RESUMED_AFTER_RESTART,
            serde_json::json!({ "boot_id": boot_id.to_string() }),
        ),
        Err(reason) => (
            NODE_RESUME_REFUSED,
            serde_json::json!({ "boot_id": boot_id.to_string(), "reason": reason }),
        ),
    };
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(node_id),
            attempt_id: Some(attempt_id),
            detail,
            ..EventSpec::execution(kind)
        },
    )?;
    if outcome.is_ok() {
        tx.execute(
            "UPDATE topology_node_attempts SET boot_id=?2,updated_at=?3 WHERE id=?1",
            params![attempt_id.to_string(), boot_id.to_string(), now_text()],
        )?;
    }
    tx.commit()?;
    Ok(update)
}

/// #2263: a `require_verified` node whose commit the daemon could not verify
/// is continued, as the same session, with its missing evidence. The event is
/// the durable claim, recorded before the provider effect: a claim whose effect
/// cannot be shown is uncertain and is never sent again.
pub(crate) const NODE_VERIFICATION_CONTINUED: &str = "node_verification_continued";
/// A claim that a typed fence refusal proved had no effect; it no longer counts.
const NODE_VERIFICATION_CONTINUATION_WITHDRAWN: &str = "node_verification_continuation_withdrawn";
/// How many times one attempt is continued with its missing evidence.
pub(crate) const MAX_VERIFICATION_CONTINUATIONS: u32 = 2;

/// #2563: a finished node that left uncommitted paths behind is continued once,
/// as the same session, to commit or discard them. Its claims are counted
/// apart from the verification ones: one never spends the other's budget.
pub(crate) const NODE_UNCOMMITTED_CONTINUED: &str = "node_uncommitted_continued";
const NODE_UNCOMMITTED_CONTINUATION_WITHDRAWN: &str = "node_uncommitted_continuation_withdrawn";
/// How many times one attempt is asked to settle its dirty tree.
pub(crate) const MAX_UNCOMMITTED_CONTINUATIONS: u32 = 1;

/// #2584: a finished node whose last message does not open with the pipeline
/// handoff marker is continued once, as the same session, with the parser's
/// error and the required first line. Counted apart from the other two.
pub(crate) const NODE_HANDOFF_FORMAT_CONTINUED: &str = "node_handoff_format_continued";
const NODE_HANDOFF_FORMAT_CONTINUATION_WITHDRAWN: &str =
    "node_handoff_format_continuation_withdrawn";
/// How many times one attempt is asked to repeat a malformed handoff.
pub(crate) const MAX_HANDOFF_FORMAT_CONTINUATIONS: u32 = 1;

/// One family of at-most-once node continuations: the durable claim event, the
/// event that takes a provably ineffective claim back, and the per-attempt cap.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ContinuationKind {
    continued: &'static str,
    withdrawn: &'static str,
    pub(crate) max: u32,
    /// Names the continuation in operator-facing text.
    pub(crate) label: &'static str,
}

pub(crate) const VERIFICATION_CONTINUATION: ContinuationKind = ContinuationKind {
    continued: NODE_VERIFICATION_CONTINUED,
    withdrawn: NODE_VERIFICATION_CONTINUATION_WITHDRAWN,
    max: MAX_VERIFICATION_CONTINUATIONS,
    label: "verification",
};

pub(crate) const UNCOMMITTED_CONTINUATION: ContinuationKind = ContinuationKind {
    continued: NODE_UNCOMMITTED_CONTINUED,
    withdrawn: NODE_UNCOMMITTED_CONTINUATION_WITHDRAWN,
    max: MAX_UNCOMMITTED_CONTINUATIONS,
    label: "uncommitted-work",
};

pub(crate) const HANDOFF_FORMAT_CONTINUATION: ContinuationKind = ContinuationKind {
    continued: NODE_HANDOFF_FORMAT_CONTINUED,
    withdrawn: NODE_HANDOFF_FORMAT_CONTINUATION_WITHDRAWN,
    max: MAX_HANDOFF_FORMAT_CONTINUATIONS,
    label: "handoff-format",
};

/// One live claim of a verification continuation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerificationContinuation {
    pub(crate) n: u32,
    /// Names the continuation's user turn in the session's conversation.
    pub(crate) delivery_id: Uuid,
}

/// The attempt's claims that were not withdrawn, oldest first.
#[cfg(test)]
pub(crate) fn verification_continuations(
    store: &Store,
    attempt_id: Uuid,
) -> Result<Vec<VerificationContinuation>> {
    continuations_of(store, attempt_id, &VERIFICATION_CONTINUATION)
}

/// The attempt's live claims of one continuation `kind`, oldest first.
pub(crate) fn continuations_of(
    store: &Store,
    attempt_id: Uuid,
    kind: &ContinuationKind,
) -> Result<Vec<VerificationContinuation>> {
    let mut statement = store.conn.prepare(
        "SELECT kind,json_extract(payload_json,'$.detail.n'),\
                json_extract(payload_json,'$.detail.delivery_id') \
         FROM topology_events WHERE attempt_id=?1 AND kind IN (?2,?3) ORDER BY execution_seq",
    )?;
    let rows = statement.query_map(
        params![attempt_id.to_string(), kind.continued, kind.withdrawn],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        },
    )?;
    let mut live: Vec<VerificationContinuation> = Vec::new();
    for row in rows {
        let (event, n, delivery_id) = row?;
        let Some(delivery_id) = delivery_id.and_then(|id| Uuid::parse_str(&id).ok()) else {
            continue;
        };
        if event == kind.withdrawn {
            live.retain(|claim| claim.delivery_id != delivery_id);
        } else {
            live.push(VerificationContinuation {
                n: u32::try_from(n.unwrap_or_default()).unwrap_or_default(),
                delivery_id,
            });
        }
    }
    Ok(live)
}

/// What claiming a verification continuation decided.
pub(crate) enum VerificationClaim {
    /// Claimed: the claim is durable; the effect may now be attempted.
    Claimed {
        continuation: VerificationContinuation,
        update: GraphExecutionUpdate,
    },
    /// The execution is cancelling or settled, or its deadline passed.
    Stopped,
    /// The attempt used all its continuations.
    Exhausted,
}

/// Record, in one transaction, that the attempt is about to be continued with
/// its missing evidence (`node_verification_continued {attempt_id, n, reason}`).
/// Rechecks that the execution is live and inside its deadline and that fewer
/// than [`MAX_VERIFICATION_CONTINUATIONS`] claims exist. The attempt stays
/// `running`; nothing about its wall time changes.
#[cfg(test)]
pub(crate) fn claim_verification_continuation(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    reason: &str,
) -> Result<VerificationClaim> {
    claim_continuation(
        store,
        execution_id,
        attempt,
        &VERIFICATION_CONTINUATION,
        reason,
    )
}

/// [`claim_verification_continuation`] for any continuation `kind`.
pub(crate) fn claim_continuation(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    kind: &ContinuationKind,
    reason: &str,
) -> Result<VerificationClaim> {
    let tx = immediate(store)?;
    let (status, deadline, attempt_status): (String, Option<String>, String) = tx.query_row(
        "SELECT e.status,e.deadline_at,a.status FROM topology_node_attempts a \
         JOIN topology_executions e ON e.id=a.execution_id WHERE a.id=?1 AND a.execution_id=?2",
        params![attempt.id.to_string(), execution_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if attempt_status != "running"
        || !matches!(status.as_str(), "accepted" | "running" | "blocked")
        || deadline
            .as_deref()
            .map(parse_time)
            .transpose()?
            .is_some_and(|deadline| deadline <= Utc::now())
    {
        return Ok(VerificationClaim::Stopped);
    }
    let claims = continuations_of(store, attempt.id, kind)?;
    let claimed = u32::try_from(claims.len()).unwrap_or(u32::MAX);
    if claimed >= kind.max {
        return Ok(VerificationClaim::Exhausted);
    }
    let continuation = VerificationContinuation {
        n: claims.last().map_or(0, |claim| claim.n) + 1,
        delivery_id: Uuid::new_v4(),
    };
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(&attempt.node_id),
            attempt_id: Some(attempt.id),
            detail: serde_json::json!({
                "attempt_id": attempt.id,
                "n": continuation.n,
                "reason": reason,
                "delivery_id": continuation.delivery_id,
            }),
            ..EventSpec::execution(kind.continued)
        },
    )?;
    tx.commit()?;
    Ok(VerificationClaim::Claimed {
        continuation,
        update,
    })
}

/// A typed fence refusal proved the continuation had no effect: take the claim
/// back so it does not count and the next completion can be continued.
pub(crate) fn withdraw_continuation(
    store: &Store,
    execution_id: Uuid,
    attempt: &AttemptRow,
    kind: &ContinuationKind,
    continuation: &VerificationContinuation,
    why: &str,
) -> Result<GraphExecutionUpdate> {
    let tx = immediate(store)?;
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(&attempt.node_id),
            attempt_id: Some(attempt.id),
            detail: serde_json::json!({
                "attempt_id": attempt.id,
                "n": continuation.n,
                "delivery_id": continuation.delivery_id,
                "why": why,
            }),
            ..EventSpec::execution(kind.withdrawn)
        },
    )?;
    tx.commit()?;
    Ok(update)
}

/// Event kinds of a node session's wait on an answer (#1641 S3b). The executor
/// opens one when it first sees the node session waiting and closes it when the
/// session is running again, so the paused wall time survives a restart and a
/// session resume (the session's own wait counter restarts with each resume).
const NODE_WAIT_STARTED: &str = "node_wait_started";
const NODE_WAIT_ENDED: &str = "node_wait_ended";

/// Time one attempt's node session has spent waiting on an answer.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NodeWaited {
    /// Total of the closed waits.
    pub(crate) closed: chrono::TimeDelta,
    /// When the still-open wait began, if there is one.
    pub(crate) open_since: Option<DateTime<Utc>>,
    /// When the latest closed wait ended: from then the node was running.
    /// Waits recovered from the session's own total do not move it: they ended
    /// at an unknown time before the observation that found them.
    pub(crate) last_ended_at: Option<DateTime<Utc>>,
    /// The session's cumulative total of ended waits (milliseconds) as of the
    /// last wait recorded, so a wait the executor never saw open (it began and
    /// ended between two observations) is the total's growth past this (#1704).
    pub(crate) session_seen_ms: i64,
}

impl NodeWaited {
    /// How much of the session's cumulative `total` the attempt has not yet
    /// accounted for. A total below what was seen means the session's counter
    /// restarted (a continue starts a new provider subprocess), so all of it
    /// is new.
    fn unseen_of(self, total: i64) -> i64 {
        if total >= self.session_seen_ms {
            total - self.session_seen_ms
        } else {
            total
        }
    }

    pub(crate) fn total(self, now: DateTime<Utc>) -> chrono::TimeDelta {
        self.closed
            + self.open_since.map_or(chrono::TimeDelta::zero(), |since| {
                (now - since).max(chrono::TimeDelta::zero())
            })
    }
}

fn node_waited_on(conn: &Connection, attempt_id: Uuid) -> Result<NodeWaited> {
    waited_on(conn, attempt_id, NODE_WAIT_STARTED, NODE_WAIT_ENDED)
}

fn build_admission_waited_on(conn: &Connection, attempt_id: Uuid) -> Result<NodeWaited> {
    waited_on(
        conn,
        attempt_id,
        "node_build_wait_started",
        "node_build_wait_ended",
    )
}

fn waited_on(
    conn: &Connection,
    attempt_id: Uuid,
    started: &str,
    ended: &str,
) -> Result<NodeWaited> {
    let mut statement = conn.prepare(
        "SELECT kind,payload_json,created_at FROM topology_events \
         WHERE attempt_id=?1 AND kind IN (?2,?3) ORDER BY execution_seq",
    )?;
    let rows = statement
        .query_map(params![attempt_id.to_string(), started, ended], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut waited = NodeWaited::default();
    for (kind, payload, at) in rows {
        if kind == started {
            // The wait began at the transition the executor recorded, which
            // may precede the event's own observation time (#1704).
            let payload: Value = serde_json::from_str(&payload)?;
            waited.open_since = Some(match payload["detail"]["since"].as_str() {
                Some(since) => parse_time(since)?,
                None => parse_time(&at)?,
            });
        } else {
            let payload: Value = serde_json::from_str(&payload)?;
            if payload["detail"]["reconciled"] != true {
                waited.last_ended_at = Some(parse_time(&at)?);
            }
            let millis = payload["detail"]["waited_ms"].as_i64().unwrap_or(0);
            waited.closed += chrono::TimeDelta::milliseconds(millis.max(0));
            waited.open_since = None;
            // A wait recorded before the session total was carried counts
            // as already covered by it, never as unseen growth.
            waited.session_seen_ms = payload["detail"]["session_waited_ms"]
                .as_i64()
                .unwrap_or_else(|| waited.session_seen_ms.max(waited.closed.num_milliseconds()));
        }
    }
    Ok(waited)
}

/// The wait accounting of one attempt, re-derived from the event log.
pub(crate) fn node_waited(store: &Store, attempt_id: Uuid) -> Result<NodeWaited> {
    node_waited_on(&store.conn, attempt_id)
}

/// Keep build-admission waits separate from answer waits: their session counters
/// are independent. Closed intervals and an open interval survive daemon boots.
pub(crate) fn account_build_admission_wait(
    store: &Store,
    execution: &ExecutionRow,
    attempt: &AttemptRow,
    since: Option<DateTime<Utc>>,
) -> Result<(chrono::TimeDelta, Option<GraphExecutionUpdate>)> {
    let tx = immediate(store)?;
    let mut waited = build_admission_waited_on(&tx, attempt.id)?;
    let now = Utc::now();
    let event = match (waited.open_since, since) {
        (None, Some(since)) => {
            let floor = attempt.started_at.unwrap_or(execution.created_at);
            let floor = waited.last_ended_at.unwrap_or(floor).max(floor);
            let since = since.clamp(floor, now.max(floor));
            waited.open_since = Some(since);
            Some((
                "node_build_wait_started",
                serde_json::json!({
                    "since": since.to_rfc3339_opts(SecondsFormat::Nanos, true),
                }),
            ))
        }
        (Some(since), None) => {
            let length = (now - since).max(chrono::TimeDelta::zero());
            waited.closed += length;
            waited.open_since = None;
            Some((
                "node_build_wait_ended",
                serde_json::json!({
                    "waited_ms": length.num_milliseconds(),
                }),
            ))
        }
        _ => None,
    };
    let update = event
        .map(|(kind, detail)| {
            append_event(
                &tx,
                execution.id,
                EventSpec {
                    node_id: Some(&attempt.node_id),
                    attempt_id: Some(attempt.id),
                    detail,
                    ..EventSpec::execution(kind)
                },
            )
        })
        .transpose()?;
    tx.commit()?;
    Ok((waited.total(now), update))
}

/// Open the attempt's wait when the node session starts waiting; `None` when
/// one is already open. The wait began at `since`, the session's own wait
/// transition time. The first observation can come long after it (a slow tick,
/// a restart), so an unknown `since` falls back to the last moment the node was
/// known not to be waiting (`floor`: the attempt start, or the end of its last
/// wait). That pauses the node more, never less, than it really waited (#1704).
pub(crate) fn record_node_wait_started(
    store: &Store,
    execution_id: Uuid,
    node_id: &str,
    attempt_id: Uuid,
    since: Option<DateTime<Utc>>,
    floor: DateTime<Utc>,
) -> Result<Option<GraphExecutionUpdate>> {
    let tx = immediate(store)?;
    let update = open_wait_in(&tx, execution_id, node_id, attempt_id, since, floor)?;
    tx.commit()?;
    Ok(update)
}

/// `record_node_wait_started` inside the caller's transaction, so a transition
/// that parks a node can open its wait atomically (#1715).
fn open_wait_in(
    tx: &Connection,
    execution_id: Uuid,
    node_id: &str,
    attempt_id: Uuid,
    since: Option<DateTime<Utc>>,
    floor: DateTime<Utc>,
) -> Result<Option<GraphExecutionUpdate>> {
    let waited = node_waited_on(tx, attempt_id)?;
    if waited.open_since.is_some() {
        return Ok(None);
    }
    let floor = waited.last_ended_at.map_or(floor, |ended| ended.max(floor));
    let began = since.unwrap_or(floor).clamp(floor, Utc::now().max(floor));
    append_event(
        tx,
        execution_id,
        EventSpec {
            node_id: Some(node_id),
            attempt_id: Some(attempt_id),
            detail: serde_json::json!({ "since": began.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true) }),
            ..EventSpec::execution(NODE_WAIT_STARTED)
        },
    )
    .map(Some)
}

/// Close the attempt's open wait, recording its length; `None` when none is
/// open. `session_waited_ms` is the session's own cumulative total of ended
/// waits: it also holds any wait that began and ended unseen before this one,
/// so the recorded length is the larger of what the executor measured and what
/// the total grew by. That can over-pause, never under-pause (#1704).
pub(crate) fn record_node_wait_ended(
    store: &Store,
    execution_id: Uuid,
    node_id: &str,
    attempt_id: Uuid,
    session_waited_ms: Option<u64>,
) -> Result<Option<GraphExecutionUpdate>> {
    let tx = immediate(store)?;
    let waited = node_waited_on(&tx, attempt_id)?;
    let Some(since) = waited.open_since else {
        return Ok(None);
    };
    let mut length = (Utc::now() - since).max(chrono::TimeDelta::zero());
    let mut detail = serde_json::Map::new();
    if let Some(total) = session_waited_ms.map(clamp_ms) {
        length = length.max(chrono::TimeDelta::milliseconds(waited.unseen_of(total)));
        detail.insert("session_waited_ms".into(), total.into());
    }
    detail.insert("waited_ms".into(), length.num_milliseconds().into());
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(node_id),
            attempt_id: Some(attempt_id),
            detail: Value::Object(detail),
            ..EventSpec::execution(NODE_WAIT_ENDED)
        },
    )?;
    tx.commit()?;
    Ok(Some(update))
}

/// Record waits the session finished without the executor seeing them open:
/// the growth of its own cumulative total past what the attempt already
/// accounts for. `None` when the total is unknown, an open wait will account
/// for it when it closes, or nothing grew. Nothing here backdates a wait's
/// start: the wait is recorded as a length, marked `reconciled` (#1704).
pub(crate) fn record_node_waits_between_observations(
    store: &Store,
    execution_id: Uuid,
    node_id: &str,
    attempt_id: Uuid,
    session_waited_ms: Option<u64>,
) -> Result<Option<GraphExecutionUpdate>> {
    let Some(total) = session_waited_ms.map(clamp_ms) else {
        return Ok(None);
    };
    let tx = immediate(store)?;
    let waited = node_waited_on(&tx, attempt_id)?;
    let unseen = waited.unseen_of(total);
    // A counter that restarted (a resumed subprocess) below the baseline is
    // recorded even when it restarted at zero: the baseline must follow it,
    // or the next wait is measured against the old total and under-credited.
    let restarted = total < waited.session_seen_ms;
    if waited.open_since.is_some() || (unseen <= 0 && !restarted) {
        return Ok(None);
    }
    let update = append_event(
        &tx,
        execution_id,
        EventSpec {
            node_id: Some(node_id),
            attempt_id: Some(attempt_id),
            detail: serde_json::json!({
                "waited_ms": unseen,
                "session_waited_ms": total,
                "reconciled": true,
            }),
            ..EventSpec::execution(NODE_WAIT_ENDED)
        },
    )?;
    tx.commit()?;
    Ok(Some(update))
}

fn clamp_ms(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
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
        waiting: if execution.status.is_final() {
            None
        } else {
            on_call_waits(store, execution_id)?.into_iter().next()
        },
        current_nodes: Vec::new(),
        on_call: None,
        rulings: Vec::new(),
        node_attempts: Vec::new(),
    }))
}

/// Settlement cleanup still owed by a terminal execution (plan §4 rules 3
/// and 5). Completion is recorded as `pins_released` / `sessions_released` /
/// `review_work_settled` events, so a crash between settling and cleaning is replayed by recovery.
pub(crate) struct CleanupDue {
    pub(crate) execution_id: Uuid,
    pub(crate) repo_root: PathBuf,
    pub(crate) pins: bool,
    pub(crate) sessions: bool,
    /// #2607: the execution's review Work rows are not settled yet.
    pub(crate) review_work: bool,
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
        "SELECT id,repo_root,pins_due,sessions_due,work_due FROM (SELECT e.id,e.repo_root,\
            (NOT EXISTS (SELECT 1 FROM topology_events v WHERE v.execution_id=e.id AND v.kind='pins_released') \
                AND (e.status='succeeded' OR e.finished_at < ?1)) AS pins_due,\
            (NOT EXISTS (SELECT 1 FROM topology_events v WHERE v.execution_id=e.id AND v.kind='sessions_released') \
                AND (e.status IN ('succeeded','cancelled') OR e.finished_at < ?1)) AS sessions_due,\
            NOT EXISTS (SELECT 1 FROM topology_events v WHERE v.execution_id=e.id AND v.kind='review_work_settled') AS work_due \
         FROM topology_executions e WHERE e.status IN ('succeeded','failed','cancelled')) \
         WHERE pins_due OR sessions_due OR work_due LIMIT ?2",
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
                    row.get::<_, bool>(4)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(id, repo, pins, sessions, review_work)| {
            Ok(CleanupDue {
                execution_id: parse_uuid(&id)?,
                repo_root: PathBuf::from(repo),
                pins,
                sessions,
                review_work,
            })
        })
        .collect()
}

/// Node sessions a settlement may archive: launched attempts only, never
/// one whose sandbox carries preserved work (only `discard` deletes that).
pub(crate) fn releasable_sessions(store: &Store, execution_id: Uuid) -> Result<Vec<Uuid>> {
    let mut statement = store.conn.prepare(
        "SELECT session_id FROM topology_node_attempts WHERE execution_id=?1 \
         AND started_at IS NOT NULL AND preserved_ref IS NULL AND session_id IS NOT NULL \
         AND node_kind NOT IN ('review','land')",
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
