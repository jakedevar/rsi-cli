//! Scoped agent topology verbs (#633, plan §5): token-bound authority, the
//! operator's model constraints and manager policy, and the store operations
//! behind the six `AgentTopology*` verbs. RPC and native-tool glue lives in
//! `session/topology_agent_verbs.rs`.
//!
//! Authority is derived only from the token-resolved caller:
//! - the current appointed manager holding `Automation`, on Epics in its
//!   live scope (effects additionally need `execute` mode and no pause);
//! - the current lead of an Epic, within that Epic only (never `discard`).
//!
//! Everyone else, including topology node sessions, is refused.

#[allow(unused_imports)]
pub(crate) use crate::store_support::config_types::{
    BULK_FANOUT_MIN_OPENROUTER_MAX, DEFAULT_BULK_FANOUT_MIN_OPENROUTER,
};
#[allow(unused_imports)]
pub(crate) use crate::store_support::topology_usage::topology_created_usage;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rsi_common::harness_manager_v2::{
    ManagerCapabilityV2, ManagerFenceV2, ManagerLaunchChoiceV2, ManagerOperatingModeV2,
    ManagerPolicyV2,
};
use rsi_common::rpc::{
    ResolveTopologyAttemptParams, ResolveTopologyAttemptResponse, TopologyAttemptAction,
};
use rsi_common::topology_agent::{
    AgentTopologyEventV1, AgentTopologyExecuteRequestV1, AgentTopologyExecuteResultV1,
    AgentTopologyExecutionSummaryV1, AgentTopologyGetExecutionRequestV1,
    AgentTopologyGetExecutionResultV1, AgentTopologyInterruptRequestV1,
    AgentTopologyInterruptResultV1, AgentTopologyListRequestV1, AgentTopologyListResultV1,
    AgentTopologyScopeV1, AgentTopologySummaryV1, AgentTopologyUpsertRequestV1,
    AgentTopologyUpsertResultV1,
};
use rsi_common::types::{
    GraphExecutionUpdate, SessionKind, SessionProvider, SessionStatus, Topology,
    TopologyDefinition, TopologyStep,
};
use rsi_graph::format::{NodeDef, WorkflowDefinition};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::harness_manager_v2::ManagerAuthorityV2;
use crate::topology::executor::{Executor, NodeEffects};
use crate::topology::resolve::resolution_error;
use crate::topology::store::{
    self as rows, Acceptance, Actor, AttemptRow, ExecutionRequester, ExecutionRow, NewExecution,
    Recorded,
};

/// Plan §5.3: session nodes of one agent-requested execution in flight.
pub(crate) const AGENT_MAX_PARALLEL_NODES: usize = 3;
const DIAGNOSTICS_MAX: usize = 32;
const DIAGNOSTIC_CHARS: usize = 512;
const EXECUTIONS_LIST_MAX: i64 = 32;

/// Operator settings the verbs read at call time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AgentKnobs {
    pub(crate) executor_enabled: bool,
    pub(crate) bulk_fanout_min_openrouter: u32,
}

// ─── errors ────────────────────────────────────────────────────────────────

/// Redacted `code`/`next_action` envelope (plan §5.1), shared with the
/// resolution surface.
pub(crate) fn refusal(code: &str, next_action: &str) -> DaemonError {
    resolution_error(code, next_action, None, None)
}

fn diagnostics_refusal(code: &str, next_action: &str, diagnostics: &[String]) -> DaemonError {
    DaemonError::StructuredRpc {
        rpc_code: rsi_common::rpc::INVALID_PARAMS,
        message: code.to_owned(),
        data: json!({
            "code": code,
            "next_action": next_action,
            "diagnostics": diagnostics,
        }),
    }
}

fn denied() -> DaemonError {
    refusal(
        "authority_denied",
        "use the current manager holding the Automation capability, or the current lead of the owning Epic",
    )
}

fn not_found() -> DaemonError {
    refusal(
        "not_found_in_scope",
        "list the topologies and executions visible in your scope and retry",
    )
}

fn invalid(code: &str) -> DaemonError {
    let next_action = match code {
        "topology_invalid_name" => "use a non-blank name of at most 128 bytes",
        "topology_invalid_idempotency_key" => {
            "supply a non-empty idempotency_key of at most 128 bytes"
        }
        "topology_invalid_expected_digest" => {
            "pass the definition_digest (sha256:<64 hex>) returned by list or upsert"
        }
        "topology_invalid_base_commit" => "pass a full 40-hex base_commit or omit it",
        "topology_invalid_inputs" => "pass inputs as a JSON object or omit it",
        "topology_invalid_limit" => "use a limit within the documented page bound",
        _ => "correct the request fields and retry",
    };
    resolution_error("invalid_params", next_action, None, None)
}

/// Collapse an internal failure into the closed envelope: typed envelopes
/// pass through; anything else (`SQLite`, filesystem, Git text) is logged and
/// replaced so no path or diagnostic crosses the agent boundary.
pub(crate) fn redact(verb: &str, error: DaemonError) -> DaemonError {
    if let DaemonError::StructuredRpc { data, .. } = &error
        && data
            .get("code")
            .and_then(serde_json::Value::as_str)
            .is_some()
    {
        return error;
    }
    tracing::warn!(verb, %error, "agent topology request failed");
    refusal(
        "request_failed",
        "retry later; if it persists, report the verb and time to the operator",
    )
}

fn clip(message: String) -> String {
    if message.chars().count() > DIAGNOSTIC_CHARS {
        message.chars().take(DIAGNOSTIC_CHARS).collect()
    } else {
        message
    }
}

fn message(error: DaemonError) -> String {
    clip(match error {
        DaemonError::InvalidParam(text) | DaemonError::PolicyDenied(text) => text,
        other => other.to_string(),
    })
}

// ─── request ledger (V133 `topology_agent_requests`) ───────────────────────

/// Canonical digest of a decoded request without its idempotency key: object
/// keys are sorted recursively, so equal requests digest equally regardless
/// of field or map order.
pub(crate) fn request_digest<T: serde::Serialize>(request: &T) -> String {
    let mut value = serde_json::to_value(request).unwrap_or(serde_json::Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.remove("idempotency_key");
    }
    rows::digest(&canonical(value).to_string())
}

fn canonical(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<String, serde_json::Value> = map
                .into_iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonical).collect())
        }
        other => other,
    }
}

/// The redacted envelope code of a refusal.
pub(crate) fn error_code(error: &DaemonError) -> String {
    match error {
        DaemonError::StructuredRpc { data, .. } => data
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("request_failed")
            .to_owned(),
        _ => "request_failed".to_owned(),
    }
}

/// What one agent topology verb call targeted, for its ledger row.
#[derive(Clone, Debug)]
pub(crate) struct CallAudit {
    pub(crate) verb: &'static str,
    pub(crate) caller: Uuid,
    pub(crate) request_digest: String,
    pub(crate) epic_id: Option<Uuid>,
    pub(crate) topology_id: Option<Uuid>,
    pub(crate) execution_id: Option<Uuid>,
}

/// How one call ended.
pub(crate) enum CallOutcome<'a> {
    Accepted(Option<serde_json::Value>),
    Deduplicated(Option<serde_json::Value>),
    Refused(&'a DaemonError),
}

/// `manager`, `epic_lead`, or `None` for a caller holding no authority.
pub(crate) fn caller_kind(store: &Store, caller: Uuid) -> Option<&'static str> {
    TopologyCaller::resolve(store, caller)
        .ok()
        .map(|caller| caller.kind())
        .or_else(|| {
            // #1235: a portfolio seat acts as a manager in its granted projects.
            store
                .portfolio_seat_grant(caller)
                .ok()
                .flatten()
                .map(|_| "manager")
        })
}

/// Append the one ledger row for a call. `key` binds a ledger-owned
/// idempotency key (upsert only) and is `None` for every other row.
pub(crate) fn record_call(
    store: &Store,
    audit: &CallAudit,
    kind: Option<&str>,
    outcome: CallOutcome<'_>,
    key: Option<&str>,
) -> Result<()> {
    use crate::store::topology_agent_audit::{AgentRequestOutcome, AgentRequestRow};
    let epic_id = audit.epic_id.or_else(|| {
        audit
            .execution_id
            .and_then(|id| rows::load_execution(store, id).ok().flatten())
            .and_then(|execution| execution.epic_id.or(execution.parent_session_id))
    });
    let (outcome, code, result) = match outcome {
        CallOutcome::Accepted(result) => (AgentRequestOutcome::Accepted, None, result),
        CallOutcome::Deduplicated(result) => (AgentRequestOutcome::Deduplicated, None, result),
        CallOutcome::Refused(error) => {
            (AgentRequestOutcome::Refused, Some(error_code(error)), None)
        }
    };
    crate::store::topology_agent_audit::insert_agent_request(
        &store.conn,
        &AgentRequestRow {
            verb: audit.verb,
            caller_session_id: audit.caller,
            caller_kind: kind,
            epic_id,
            topology_id: audit.topology_id,
            execution_id: audit.execution_id,
            idempotency_key: key,
            request_digest: &audit.request_digest,
            outcome,
            code: code.as_deref(),
            result_json: result.map(|value| value.to_string()),
        },
    )
}

/// Append the one ledger row for a finished call; `Ok((receipt, true))` is a
/// replay. Best effort once an effect committed: a failure is logged.
pub(crate) fn record_result(
    store: &Store,
    audit: &CallAudit,
    outcome: std::result::Result<(Option<serde_json::Value>, bool), &DaemonError>,
) {
    let kind = caller_kind(store, audit.caller);
    let call = match outcome {
        Ok((receipt, true)) => CallOutcome::Deduplicated(receipt),
        Ok((receipt, false)) => CallOutcome::Accepted(receipt),
        Err(error) => CallOutcome::Refused(error),
    };
    if let Err(error) = record_call(store, audit, kind, call, None) {
        tracing::warn!(%error, verb = audit.verb, "agent topology request ledger write failed");
    }
}

// ─── authority ─────────────────────────────────────────────────────────────

/// How far a request reaches into an Epic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Reach {
    /// Reads and authoring: scope only.
    Read,
    /// Launching or resuming work: a manager also needs `execute` mode and
    /// no operator pause.
    Effect,
    /// Stopping work: scope only (a paused manager can still stop work).
    Stop,
}

/// The token-resolved caller of an `AgentTopology*` verb.
#[derive(Clone, Debug)]
pub(crate) enum TopologyCaller {
    Manager {
        session: Uuid,
        authority: Box<ManagerAuthorityV2>,
    },
    Lead {
        session: Uuid,
        epic_id: Uuid,
        project_id: Option<Uuid>,
    },
}

impl TopologyCaller {
    /// Resolve the caller from durable rows only; never from request fields.
    pub(crate) fn resolve(store: &Store, caller: Uuid) -> Result<Self> {
        Self::resolve_in(store, caller, None)
    }

    /// [`Self::resolve`] with an optional target project (#1235): omitted or
    /// the caller's own project keeps the lead-then-manager order; the
    /// global seat is served inside its grant; any other project is refused
    /// `manager_project_not_in_scope`. The project is a target, not identity.
    pub(crate) fn resolve_in(store: &Store, caller: Uuid, project: Option<Uuid>) -> Result<Self> {
        let own = store
            .get_session(caller)?
            .and_then(|session| session.project_id);
        let foreign = project.filter(|project| own != Some(*project));
        if foreign.is_none()
            && let Some(lead) = Self::lead(store, caller)?
        {
            return Ok(lead);
        }
        let config = match store.resolve_manager_caller(caller, project) {
            Ok(crate::store::harness_manager_v2::ManagerCallerV1::Legacy {
                config,
                is_manager: true,
            }) => config,
            Ok(crate::store::harness_manager_v2::ManagerCallerV1::Global(authority)) => {
                authority.config
            }
            Err(DaemonError::InvalidParam(code))
                if code == rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE =>
            {
                return Err(refusal(
                    rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE,
                    "name a project in your global grant, or omit project_id for your own project",
                ));
            }
            _ => return Err(denied()),
        };
        let grant = store
            .manager_policy_for_config(&config)
            .map_err(|_| denied())?
            .ok_or_else(denied)?;
        let fence = ManagerFenceV2 {
            scope_version: config.row_version,
            policy_version: grant.row_version,
        };
        let authority = store
            .manager_v2_authorize_in(
                caller,
                Some(config.project_id),
                &fence,
                Some(ManagerCapabilityV2::Automation),
            )
            .map_err(|error| match error {
                DaemonError::InvalidParam(code) if code == "manager_v2_capability_denied" => {
                    refusal(
                        "capability_denied",
                        "ask the operator to grant the manager the Automation capability",
                    )
                }
                _ => denied(),
            })?;
        Ok(Self::Manager {
            session: caller,
            authority: Box::new(authority),
        })
    }

    /// The current lead of the Epic that directly owns the caller.
    fn lead(store: &Store, caller: Uuid) -> Result<Option<Self>> {
        let live = |status: SessionStatus| {
            !matches!(status, SessionStatus::Archived | SessionStatus::Deleted)
        };
        let Some(session) = store.get_session(caller)? else {
            return Ok(None);
        };
        if !rsi_common::is_leaf_kind(session.session_kind) || !live(session.status) {
            return Ok(None);
        }
        let Some(epic) = session
            .parent_id
            .map(|parent| store.get_session(parent))
            .transpose()?
            .flatten()
        else {
            return Ok(None);
        };
        if epic.session_kind != SessionKind::Epic
            || epic.lead_session_id != Some(caller)
            || !live(epic.status)
        {
            return Ok(None);
        }
        Ok(Some(Self::Lead {
            session: caller,
            epic_id: epic.id,
            project_id: epic.project_id,
        }))
    }

    /// Ledger `caller_kind`.
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::Manager { .. } => "manager",
            Self::Lead { .. } => "epic_lead",
        }
    }

    pub(crate) const fn session(&self) -> Uuid {
        match self {
            Self::Manager { session, .. } | Self::Lead { session, .. } => *session,
        }
    }

    pub(crate) const fn actor(&self) -> Actor {
        match self {
            Self::Manager { session, .. } => Actor::manager(*session),
            Self::Lead { session, .. } => Actor::epic_lead(*session),
        }
    }

    fn project_id(&self) -> Option<Uuid> {
        match self {
            Self::Manager { authority, .. } => Some(authority.config.project_id),
            Self::Lead { project_id, .. } => *project_id,
        }
    }

    /// Epics this caller may act on.
    fn epics(&self) -> Vec<Uuid> {
        match self {
            Self::Manager { authority, .. } => authority.config.epic_ids.clone(),
            Self::Lead { epic_id, .. } => vec![*epic_id],
        }
    }

    /// The operator's live model constraints for this caller: the manager's
    /// own grant, or (for a lead) its project's grant when one is in force.
    /// `None` means no constraint was granted, which fails closed.
    fn policy(&self, store: &Store) -> Result<Option<ManagerPolicyV2>> {
        let (mut policy, config) = match self {
            Self::Manager { authority, .. } => (
                Some(authority.grant.policy.clone()),
                Some(authority.config.clone()),
            ),
            Self::Lead { project_id, .. } => (
                project_id
                    .map(|project| store.get_harness_manager_policy(project))
                    .transpose()?
                    .flatten()
                    .filter(|grant| !grant.revoked)
                    .map(|grant| grant.policy),
                None,
            ),
        };
        let config = match config {
            Some(config) => Some(config),
            None => self
                .project_id()
                .map(|project| store.get_harness_manager(project))
                .transpose()?
                .flatten(),
        };
        if let (Some(policy), Some(config)) = (&mut policy, config) {
            policy.allowed_launches =
                store.manager_effective_launches(&config, &policy.allowed_launches)?;
        }
        Ok(policy)
    }

    /// Require `epic_id` inside this caller's scope at `reach`.
    pub(crate) fn require_epic(&self, store: &Store, epic_id: Uuid, reach: Reach) -> Result<()> {
        match self {
            Self::Lead { epic_id: own, .. } => {
                if *own == epic_id {
                    Ok(())
                } else {
                    Err(not_found())
                }
            }
            Self::Manager { authority, .. } => {
                if !authority.config.epic_ids.contains(&epic_id) {
                    return Err(not_found());
                }
                store
                    .manager_v2_require_epic(authority, epic_id)
                    .map_err(|_| not_found())?;
                let policy = &authority.grant.policy;
                if reach == Reach::Effect {
                    if policy.mode != ManagerOperatingModeV2::Execute {
                        return Err(refusal(
                            "manager_not_execute",
                            "the operator must set the manager policy mode to execute",
                        ));
                    }
                    if policy.paused || policy.paused_epic_ids.contains(&epic_id) {
                        return Err(refusal(
                            "paused",
                            "the operator paused the manager or this Epic; wait for the resume",
                        ));
                    }
                }
                Ok(())
            }
        }
    }

    /// Load an execution that runs under an Epic in this caller's scope.
    pub(crate) fn require_execution(
        &self,
        store: &Store,
        execution_id: Uuid,
        reach: Reach,
    ) -> Result<ExecutionRow> {
        let execution = rows::load_execution(store, execution_id)?.ok_or_else(not_found)?;
        let epic = execution
            .epic_id
            .or(execution.parent_session_id)
            .ok_or_else(not_found)?;
        self.require_epic(store, epic, reach)?;
        Ok(execution)
    }

    /// Topology visibility (plan §5.2, open question 3: operator topologies
    /// only when `shared`).
    fn sees(&self, record: &TopologyRecord) -> bool {
        match self {
            Self::Lead {
                epic_id,
                project_id,
                ..
            } => match record.owner_kind.as_str() {
                "epic" => record.epic_id == Some(*epic_id),
                "manager" => {
                    record.shared && record.project_id.is_some() && record.project_id == *project_id
                }
                "operator" => record.shared,
                _ => false,
            },
            Self::Manager { authority, .. } => {
                let project = Some(authority.config.project_id);
                match record.owner_kind.as_str() {
                    "epic" => {
                        record.project_id == project
                            && record
                                .epic_id
                                .is_some_and(|epic| authority.config.epic_ids.contains(&epic))
                    }
                    "manager" => record.project_id == project,
                    "operator" => record.shared,
                    _ => false,
                }
            }
        }
    }

    /// An Epic-owned topology runs only on its own Epic.
    fn may_execute(&self, record: &TopologyRecord, epic_id: Uuid) -> bool {
        self.sees(record) && (record.owner_kind != "epic" || record.epic_id == Some(epic_id))
    }
}

// ─── topology rows ─────────────────────────────────────────────────────────

/// One `topologies` row with its V129 ownership columns.
#[derive(Clone, Debug)]
pub(crate) struct TopologyRecord {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    definition_json: String,
    pub(crate) definition: TopologyDefinition,
    pub(crate) owner_kind: String,
    pub(crate) project_id: Option<Uuid>,
    pub(crate) epic_id: Option<Uuid>,
    pub(crate) revision: i64,
    pub(crate) shared: bool,
    archived: bool,
    created_at: String,
    updated_at: String,
}

const TOPOLOGY_COLUMNS: &str = "id,name,definition_json,owner_kind,project_id,epic_id,revision,shared,archived_at,created_at,updated_at";

type RawTopology = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    Option<String>,
    String,
    String,
);

fn raw_topology(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawTopology> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn uuid_column(text: Option<String>) -> Result<Option<Uuid>> {
    text.map(|text| {
        Uuid::parse_str(&text).map_err(|_| DaemonError::Store("invalid topology UUID".into()))
    })
    .transpose()
}

impl TopologyRecord {
    fn from_raw(raw: RawTopology) -> Result<Self> {
        let (
            id,
            name,
            definition_json,
            owner_kind,
            project,
            epic,
            revision,
            shared,
            archived,
            created_at,
            updated_at,
        ) = raw;
        Ok(Self {
            id: Uuid::parse_str(&id)
                .map_err(|_| DaemonError::Store("invalid topology UUID".into()))?,
            name,
            definition: serde_json::from_str(&definition_json)?,
            definition_json,
            owner_kind,
            project_id: uuid_column(project)?,
            epic_id: uuid_column(epic)?,
            revision,
            shared: shared != 0,
            archived: archived.is_some(),
            created_at,
            updated_at,
        })
    }

    fn load(conn: &Connection, id: Uuid) -> Result<Option<Self>> {
        conn.query_row(
            &format!("SELECT {TOPOLOGY_COLUMNS} FROM topologies WHERE id=?1"),
            [id.to_string()],
            raw_topology,
        )
        .optional()?
        .map(Self::from_raw)
        .transpose()
    }

    fn by_name(conn: &Connection, name: &str) -> Result<Option<Self>> {
        conn.query_row(
            &format!("SELECT {TOPOLOGY_COLUMNS} FROM topologies WHERE name=?1"),
            [name],
            raw_topology,
        )
        .optional()?
        .map(Self::from_raw)
        .transpose()
    }

    /// Digest of the exact stored definition text. Operator updates rewrite
    /// the text without touching the V129 column, so it is never cached.
    pub(crate) fn digest(&self) -> String {
        rows::digest(&self.definition_json)
    }

    fn summary(&self) -> AgentTopologySummaryV1 {
        AgentTopologySummaryV1 {
            topology_id: self.id,
            name: self.name.clone(),
            owner_kind: self.owner_kind.clone(),
            epic_id: self.epic_id,
            revision: self.revision,
            definition_digest: self.digest(),
            shared: self.shared,
            updated_at: self.updated_at.clone(),
        }
    }

    fn topology(&self) -> Topology {
        let time = |text: &str| {
            DateTime::parse_from_rfc3339(text)
                .map_or_else(|_| Utc::now(), |t| t.with_timezone(&Utc))
        };
        Topology {
            id: self.id,
            name: self.name.clone(),
            definition: self.definition.clone(),
            created_at: time(&self.created_at),
            updated_at: time(&self.updated_at),
        }
    }
}

/// Operator-only: mark a topology agent-executable (`shared`, plan §5.2).
pub(crate) fn set_shared(store: &Store, topology_id: Uuid, shared: bool) -> Result<bool> {
    Ok(store.conn.execute(
        "UPDATE topologies SET shared=?2,updated_at=?3 WHERE id=?1",
        params![topology_id.to_string(), i64::from(shared), rows::now_text()],
    )? == 1)
}

// ─── definition policy (plan §3 rules, §5.3) ───────────────────────────────

/// The explicit `(provider, model, effort)` a session node launches with,
/// derived exactly as the launcher derives it (`topology/launch.rs`).
pub(crate) fn node_launch(node: &NodeDef) -> Option<ManagerLaunchChoiceV2> {
    let provider = crate::session::graph_runner::resolve_provider(node)?;
    let model = node
        .model_settings
        .as_ref()
        .and_then(|settings| settings.model.clone())
        .filter(|model| !model.trim().is_empty())?;
    let effort = node
        .tags
        .iter()
        .find_map(|tag| tag.strip_prefix("effort="))
        .filter(|effort| !effort.trim().is_empty())?
        .to_owned();
    Some(ManagerLaunchChoiceV2 {
        provider,
        model,
        effort: Some(effort),
    })
}

/// The equality rule of manager launches (`manager_action_resources`); an
/// empty grant list fails closed for agents.
pub(crate) fn launch_granted(
    allowed: &[ManagerLaunchChoiceV2],
    launch: &ManagerLaunchChoiceV2,
) -> bool {
    allowed.iter().any(|choice| {
        choice.provider == launch.provider
            && choice.model == launch.model
            && choice.effort == launch.effort
    })
}

/// Plan §5.3: a reviewer's vendor family must differ from the author's. An
/// unknown family on either side cannot prove independence and fails
/// closed. Review nodes are refused at validation until T3b (plan §3 rule
/// 9), which wires this check into review-node validation.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn reviewer_family_independent(
    author: &ManagerLaunchChoiceV2,
    reviewer: &ManagerLaunchChoiceV2,
) -> bool {
    use rsi_common::model_utils::vendor_family;
    match (
        vendor_family(author.provider, &author.model),
        vendor_family(reviewer.provider, &reviewer.model),
    ) {
        (Some(author), Some(reviewer)) => author != reviewer,
        _ => false,
    }
}

fn is_session_node(steps: &crate::topology::steps::WorkflowSteps, node: &str) -> bool {
    matches!(steps.step(node), None | Some(TopologyStep::Session { .. }))
}

/// Diagnostics for one definition under the caller's model constraints.
pub(crate) struct Checked {
    pub(crate) workflow: Option<WorkflowDefinition>,
    /// Structural problems (plan §3 static validation, rule 10 custody).
    pub(crate) structural: Vec<String>,
    /// Model-constraint problems (plan §5.3).
    pub(crate) policy: Vec<String>,
    pub(crate) session_nodes: usize,
}

impl Checked {
    fn all(&self) -> Vec<String> {
        self.structural
            .iter()
            .chain(self.policy.iter())
            .take(DIAGNOSTICS_MAX)
            .cloned()
            .collect()
    }

    fn refusal(&self) -> Option<DaemonError> {
        if !self.structural.is_empty() {
            Some(diagnostics_refusal(
                "invalid_definition",
                "fix the reported definition problems; validate_only reports them without writing",
                &self.all(),
            ))
        } else if !self.policy.is_empty() {
            Some(diagnostics_refusal(
                "policy_refused",
                "use explicit provider/model/effort triples the operator granted in allowed_launches",
                &self.all(),
            ))
        } else {
            None
        }
    }
}

/// Run the upsert/execute static validation (plan §3 rules 1–10, the whole-
/// workflow custody plan) and the model constraints (plan §5.3).
pub(crate) fn check_definition(
    topology: &Topology,
    allowed: &[ManagerLaunchChoiceV2],
    bulk_fanout_min_openrouter: u32,
) -> Checked {
    let mut checked = Checked {
        workflow: None,
        structural: Vec::new(),
        policy: Vec::new(),
        session_nodes: 0,
    };
    if let Err(error) =
        crate::session::topology_ops::validate_topology_definition(&topology.definition)
    {
        checked.structural.push(message(error.into()));
        return checked;
    }
    let workflow = match crate::session::SessionManager::bridge_topology(topology) {
        Ok(workflow) => workflow,
        Err(error) => {
            checked.structural.push(message(error.into()));
            return checked;
        }
    };
    let steps = match crate::session::graph_executions::validate_live_workflow(&workflow) {
        Ok(steps) => steps,
        Err(error) => {
            checked.structural.push(message(error));
            return checked;
        }
    };
    if let Err(error) = crate::session::graph_runner::plan_workflow_custody(&workflow) {
        checked.structural.push(message(error));
    }
    let sessions: Vec<&NodeDef> = workflow
        .nodes
        .iter()
        .filter(|node| is_session_node(&steps, &node.id))
        .collect();
    checked.session_nodes = sessions.len();
    if !sessions.is_empty() && allowed.is_empty() {
        checked.policy.push(
            "no allowed_launches are granted by the operator; agent session nodes fail closed"
                .into(),
        );
    }
    for node in &sessions {
        match node_launch(node) {
            None => checked.policy.push(format!(
                "node {}: a session node needs an explicit provider, model and effort",
                node.id
            )),
            Some(launch) if !allowed.is_empty() && !launch_granted(allowed, &launch) => {
                checked.policy.push(clip(format!(
                    "node {}: launch {:?}/{}/{} is not in the operator's allowed_launches",
                    node.id,
                    launch.provider,
                    launch.model,
                    launch.effort.as_deref().unwrap_or("")
                )));
            }
            Some(_) => {}
        }
    }
    checked.policy.extend(fanout_diagnostics(
        &topology.definition,
        &workflow,
        &steps,
        bulk_fanout_min_openrouter,
    ));
    checked.workflow = Some(workflow);
    checked
}

/// Plan §5.3 bulk rule: a layer (longest forward path from a source) with
/// at least `min` session nodes of the same kind must run on `OpenRouter`.
fn fanout_diagnostics(
    definition: &TopologyDefinition,
    workflow: &WorkflowDefinition,
    steps: &crate::topology::steps::WorkflowSteps,
    min: u32,
) -> Vec<String> {
    if min == 0 {
        return Vec::new();
    }
    let forward: Vec<(&str, &str)> = definition
        .edges
        .iter()
        .filter(|edge| !edge.loop_edge)
        .map(|edge| (edge.from.as_str(), edge.to.as_str()))
        .collect();
    let mut depth: HashMap<&str, u32> = definition
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), 0))
        .collect();
    // Longest path by bounded relaxation (validated acyclic without loop
    // edges, so |nodes| rounds suffice).
    for _ in 0..definition.nodes.len() {
        let mut changed = false;
        for (from, to) in &forward {
            let next = depth.get(from).copied().unwrap_or(0) + 1;
            if depth.get(to).copied().unwrap_or(0) < next {
                depth.insert(to, next);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let providers: HashMap<&str, Option<SessionProvider>> = workflow
        .nodes
        .iter()
        .map(|node| {
            (
                node.id.as_str(),
                crate::session::graph_runner::resolve_provider(node),
            )
        })
        .collect();
    let mut layers: BTreeMap<(u32, String), Vec<&str>> = BTreeMap::new();
    for node in &definition.nodes {
        if !is_session_node(steps, &node.id) {
            continue;
        }
        layers
            .entry((depth[node.id.as_str()], format!("{:?}", node.kind)))
            .or_default()
            .push(node.id.as_str());
    }
    let mut diagnostics = Vec::new();
    for ((layer, kind), nodes) in layers {
        if u32::try_from(nodes.len()).unwrap_or(u32::MAX) < min {
            continue;
        }
        let off: Vec<&str> = nodes
            .iter()
            .copied()
            .filter(|node| {
                providers.get(node).copied().flatten() != Some(SessionProvider::OpenRouter)
            })
            .collect();
        if !off.is_empty() {
            diagnostics.push(clip(format!(
                "layer {layer}: {} parallel {kind} session nodes need provider openrouter \
                 (topology_bulk_fanout_min_openrouter={min}); not openrouter: {}",
                nodes.len(),
                off.join(", ")
            )));
        }
    }
    diagnostics
}

// ─── launch-time gate (plan §5.3) ──────────────────────────────────────────

/// Re-check live policy before one agent-requested session launch. Returns
/// the refusal code, or `None` to launch. Epic parenting applies the
/// active, provider and spend caps inside the launch itself.
fn global_launch_gate(
    store: &Store,
    execution: &ExecutionRow,
    attempt: &AttemptRow,
    project: Uuid,
    requester: Uuid,
    launch: &ManagerLaunchChoiceV2,
) -> Result<Option<&'static str>> {
    let Ok(crate::store::harness_manager_v2::ManagerCallerV1::Global(authority)) =
        store.resolve_manager_caller(requester, Some(project))
    else {
        return Ok(Some("manager_scope_changed"));
    };
    let policy = &authority.grant.policy;
    let allowed = store.manager_effective_launches(&authority.config, &policy.allowed_launches)?;
    if !launch_granted(&allowed, launch) {
        return Ok(Some("launch_not_granted"));
    }
    if execution.scope_version != Some(authority.config.row_version) {
        return Ok(Some("manager_scope_changed"));
    }
    if !policy
        .capabilities
        .contains(&ManagerCapabilityV2::Automation)
    {
        return Ok(Some("capability_denied"));
    }
    let epic = execution.epic_id.or(execution.parent_session_id);
    if epic.is_none_or(|epic| !authority.config.epic_ids.contains(&epic)) {
        return Ok(Some("epic_out_of_scope"));
    }
    if policy.mode != ManagerOperatingModeV2::Execute
        || policy.paused
        || epic.is_some_and(|epic| policy.paused_epic_ids.contains(&epic))
    {
        return Ok(Some("manager_paused"));
    }
    let usage = store.manager_v2_created_usage(&authority.config, false)?;
    let own = i64::from(attempt.boot_id.is_some());
    if usage - own >= i64::from(policy.max_created_sessions) {
        return Ok(Some("creation_limit"));
    }
    ancestor_allowance(store, &authority.config, 1, own)
}

/// #1275: a topology session launch is charged against every ancestor's
/// allowance too, exactly as a lifecycle `create_session` is. `counted` is
/// the launches already charged (a relaunch of this attempt).
fn ancestor_allowance(
    store: &Store,
    config: &rsi_common::harness_manager::HarnessManagerConfigV1,
    needed: i64,
    counted: i64,
) -> Result<Option<&'static str>> {
    match store.manager_ancestor_creation_allowance(config, false, needed, counted) {
        Ok(()) => Ok(None),
        Err(DaemonError::InvalidParam(code)) if code == "manager_v2_creation_limit" => {
            Ok(Some("creation_limit"))
        }
        Err(DaemonError::InvalidParam(code))
            if code == rsi_common::global_manager::MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED =>
        {
            Ok(Some(
                rsi_common::global_manager::MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED,
            ))
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn launch_gate(
    store: &Store,
    execution: &ExecutionRow,
    attempt: &AttemptRow,
) -> Result<Option<&'static str>> {
    let Some(project) = execution.project_id else {
        return Ok(Some("project_required"));
    };
    let Some(node) = execution
        .definition
        .nodes
        .iter()
        .find(|node| node.id == attempt.node_id)
    else {
        return Ok(Some("node_missing"));
    };
    let Some(launch) = node_launch(node) else {
        return Ok(Some("launch_not_explicit"));
    };
    // #1235: an execution the global seat requested is governed only by its
    // grant, re-resolved here; a rotated, revoked or replaced seat's
    // execution never falls back to the project manager's policy.
    if execution.requested_by_kind == "manager"
        && let Some(requester) = execution.requested_by_session_id
        && store.is_global_seat_ever(requester)?
    {
        return global_launch_gate(store, execution, attempt, project, requester, &launch);
    }
    let grant = store
        .get_harness_manager_policy(project)?
        .filter(|grant| !grant.revoked);
    let Some(grant) = grant else {
        return Ok(Some("launch_not_granted"));
    };
    let config = store.get_harness_manager(project)?;
    let allowed = match &config {
        Some(config) => store.manager_effective_launches(config, &grant.policy.allowed_launches)?,
        None => grant.policy.allowed_launches.clone(),
    };
    if !launch_granted(&allowed, &launch) {
        return Ok(Some("launch_not_granted"));
    }
    if execution.requested_by_kind != "manager" {
        return Ok(None);
    }
    let Some(config) = config else {
        return Ok(Some("manager_scope_changed"));
    };
    let policy = &grant.policy;
    let epic = execution.epic_id.or(execution.parent_session_id);
    if execution.scope_version != Some(config.row_version) {
        return Ok(Some("manager_scope_changed"));
    }
    if !policy
        .capabilities
        .contains(&ManagerCapabilityV2::Automation)
    {
        return Ok(Some("capability_denied"));
    }
    if epic.is_none_or(|epic| !config.epic_ids.contains(&epic)) {
        return Ok(Some("epic_out_of_scope"));
    }
    if policy.mode != ManagerOperatingModeV2::Execute
        || policy.paused
        || epic.is_some_and(|epic| policy.paused_epic_ids.contains(&epic))
    {
        return Ok(Some("manager_paused"));
    }
    let usage = store.manager_v2_created_usage(&config, false)?;
    // A relaunch of this very attempt is already counted.
    let own = i64::from(attempt.boot_id.is_some());
    if usage - own >= i64::from(policy.max_created_sessions) {
        return Ok(Some("creation_limit"));
    }
    ancestor_allowance(store, &config, 1, own)
}

// ─── verbs ─────────────────────────────────────────────────────────────────

/// `AgentTopologyUpsert` (plan §5.1). Idempotency is content-addressed: an
/// identical definition under the same name and owner replays without a
/// write, and a revision needs the current `expected_revision`.
pub(crate) fn upsert(
    store: &Store,
    knobs: AgentKnobs,
    caller_id: Uuid,
    request: &AgentTopologyUpsertRequestV1,
) -> Result<(AgentTopologyUpsertResultV1, Option<Topology>)> {
    let digest = request_digest(request);
    let mut audit = CallAudit {
        verb: "upsert",
        caller: caller_id,
        request_digest: digest.clone(),
        epic_id: request.epic_id,
        topology_id: None,
        execution_id: None,
    };
    // The key lookup, the topology write and the ledger row commit together.
    let tx = rusqlite::Transaction::new_unchecked(
        &store.conn,
        rusqlite::TransactionBehavior::Immediate,
    )?;
    match upsert_in_tx(store, knobs, caller_id, request, &digest) {
        Ok(done) => {
            audit.epic_id = done.epic_id;
            audit.topology_id = done.result.topology_id;
            let receipt = Some(serde_json::to_value(&done.result)?);
            let (outcome, key) = match done.ledger {
                Ledger::Bind => (
                    if done.result.deduplicated {
                        CallOutcome::Deduplicated(receipt)
                    } else {
                        CallOutcome::Accepted(receipt)
                    },
                    Some(request.idempotency_key.as_str()),
                ),
                Ledger::Replay => (CallOutcome::Deduplicated(receipt), None),
                Ledger::Unbound => (CallOutcome::Accepted(receipt), None),
            };
            record_call(store, &audit, Some(done.kind), outcome, key)?;
            tx.commit()?;
            Ok((done.result, done.written))
        }
        Err(error) => {
            drop(tx);
            let kind = caller_kind(store, caller_id);
            if let Err(audit_error) =
                record_call(store, &audit, kind, CallOutcome::Refused(&error), None)
            {
                tracing::warn!(%audit_error, "agent topology upsert refusal audit failed");
            }
            Err(error)
        }
    }
}

/// How an upsert relates to its idempotency key.
enum Ledger {
    /// First use of the key: the ledger row binds it.
    Bind,
    /// The key replays its bound receipt; nothing is written.
    Replay,
    /// `validate_only`: nothing is written and the key stays unbound.
    Unbound,
}

struct UpsertDone {
    result: AgentTopologyUpsertResultV1,
    written: Option<Topology>,
    ledger: Ledger,
    epic_id: Option<Uuid>,
    kind: &'static str,
}

#[allow(clippy::too_many_lines)]
fn upsert_in_tx(
    store: &Store,
    knobs: AgentKnobs,
    caller_id: Uuid,
    request: &AgentTopologyUpsertRequestV1,
    digest: &str,
) -> Result<UpsertDone> {
    request.validate().map_err(invalid)?;
    let caller = TopologyCaller::resolve_in(store, caller_id, request.project_id)?;
    let (epic_id, project_id) = match (request.scope, &caller) {
        (
            AgentTopologyScopeV1::Epic,
            TopologyCaller::Lead {
                epic_id,
                project_id,
                ..
            },
        ) => {
            if request
                .epic_id
                .is_some_and(|requested| requested != *epic_id)
            {
                return Err(not_found());
            }
            (Some(*epic_id), *project_id)
        }
        (AgentTopologyScopeV1::Epic, TopologyCaller::Manager { authority, .. }) => {
            let epic = request.epic_id.ok_or_else(|| {
                refusal(
                    "invalid_params",
                    "an epic-scope upsert by the manager names epic_id",
                )
            })?;
            caller.require_epic(store, epic, Reach::Read)?;
            (Some(epic), Some(authority.config.project_id))
        }
        (AgentTopologyScopeV1::Manager, TopologyCaller::Lead { .. }) => return Err(denied()),
        (AgentTopologyScopeV1::Manager, TopologyCaller::Manager { authority, .. }) => {
            if request.epic_id.is_some() {
                return Err(refusal(
                    "invalid_params",
                    "a manager-scope topology names no epic_id",
                ));
            }
            (None, Some(authority.config.project_id))
        }
    };
    if !request.validate_only
        && let Some(bound) = crate::store::topology_agent_audit::bound_request(
            &store.conn,
            caller_id,
            "upsert",
            &request.idempotency_key,
        )?
    {
        if bound.request_digest != digest {
            return Err(refusal(
                "idempotency_conflict",
                "retry the original request unchanged or use a new idempotency_key",
            ));
        }
        let mut result: AgentTopologyUpsertResultV1 = bound
            .result_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .ok_or_else(|| {
                refusal(
                    "request_failed",
                    "retry later; if it persists, report the verb and time to the operator",
                )
            })?;
        result.deduplicated = true;
        return Ok(UpsertDone {
            result,
            written: None,
            ledger: Ledger::Replay,
            epic_id,
            kind: caller.kind(),
        });
    }
    let owner_kind = request.scope.owner_kind();
    let existing = TopologyRecord::by_name(&store.conn, &request.name)?;
    let owned = match existing {
        Some(record)
            if record.owner_kind == owner_kind
                && record.epic_id == epic_id
                && record.project_id == project_id
                && !record.archived =>
        {
            Some(record)
        }
        Some(_) => {
            return Err(refusal(
                "name_conflict",
                "topology names are unique across the daemon; choose a different name",
            ));
        }
        None => None,
    };
    let now = rows::now_text();
    let topology = Topology {
        id: owned.as_ref().map_or_else(Uuid::new_v4, |record| record.id),
        name: request.name.clone(),
        definition: request.definition.clone(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let allowed = caller
        .policy(store)?
        .map(|policy| policy.allowed_launches)
        .unwrap_or_default();
    let checked = check_definition(&topology, &allowed, knobs.bulk_fanout_min_openrouter);
    let definition_json = serde_json::to_string(&request.definition)?;
    let definition_digest = rows::digest(&definition_json);
    if request.validate_only {
        return Ok(UpsertDone {
            result: AgentTopologyUpsertResultV1 {
                topology_id: owned.as_ref().map(|record| record.id),
                revision: owned.as_ref().map(|record| record.revision),
                definition_digest,
                diagnostics: checked.all(),
                deduplicated: false,
            },
            written: None,
            ledger: Ledger::Unbound,
            epic_id,
            kind: caller.kind(),
        });
    }
    if let Some(refused) = checked.refusal() {
        return Err(refused);
    }
    let caller_id = caller.session().to_string();
    let (id, revision, stored_digest, deduplicated) = match owned {
        Some(record) if record.definition == request.definition => {
            (record.id, record.revision, record.digest(), true)
        }
        Some(record) => {
            if request.expected_revision != Some(record.revision) {
                return Err(DaemonError::StructuredRpc {
                    rpc_code: rsi_common::rpc::INVALID_PARAMS,
                    message: "stale_revision".into(),
                    data: json!({
                        "code": "stale_revision",
                        "next_action": "list the topology and retry with its current revision as expected_revision",
                        "expected_revision": request.expected_revision,
                        "actual_revision": record.revision,
                    }),
                });
            }
            let changed = store.conn.execute(
                "UPDATE topologies SET definition_json=?2,definition_digest=?3,revision=revision+1,\
                    owner_session_id=?4,updated_at=?5 WHERE id=?1 AND revision=?6",
                params![
                    record.id.to_string(),
                    definition_json,
                    definition_digest,
                    caller_id,
                    now,
                    record.revision,
                ],
            )?;
            if changed != 1 {
                return Err(refusal(
                    "stale_revision",
                    "list the topology and retry with its current revision as expected_revision",
                ));
            }
            (record.id, record.revision + 1, definition_digest, false)
        }
        None => {
            if request.expected_revision.is_some() {
                return Err(not_found());
            }
            let inserted = store.conn.execute(
                "INSERT INTO topologies (id,name,definition_json,created_at,updated_at,owner_kind,\
                    owner_session_id,project_id,epic_id,revision,definition_digest,shared) \
                 VALUES (?1,?2,?3,?4,?4,?5,?6,?7,?8,1,?9,0)",
                params![
                    topology.id.to_string(),
                    request.name,
                    definition_json,
                    now,
                    owner_kind,
                    caller_id,
                    project_id.map(|id| id.to_string()),
                    epic_id.map(|id| id.to_string()),
                    definition_digest,
                ],
            );
            match inserted {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(refusal(
                        "name_conflict",
                        "topology names are unique across the daemon; choose a different name",
                    ));
                }
                Err(error) => return Err(error.into()),
            }
            (topology.id, 1, definition_digest, false)
        }
    };
    Ok(UpsertDone {
        result: AgentTopologyUpsertResultV1 {
            topology_id: Some(id),
            revision: Some(revision),
            definition_digest: stored_digest,
            diagnostics: Vec::new(),
            deduplicated,
        },
        written: (!deduplicated).then_some(Topology { id, ..topology }),
        ledger: Ledger::Bind,
        epic_id,
        kind: caller.kind(),
    })
}

/// `AgentTopologyList` (plan §5.1): a name-ordered page of the topologies
/// visible to the caller, optionally with its scope's live executions.
pub(crate) fn list(
    store: &Store,
    caller: Uuid,
    request: &AgentTopologyListRequestV1,
) -> Result<AgentTopologyListResultV1> {
    let limit = request.validated_limit().map_err(invalid)?;
    let caller = TopologyCaller::resolve_in(store, caller, request.project_id)?;
    if let Some(epic) = request.epic_id {
        caller.require_epic(store, epic, Reach::Read)?;
    }
    let epics = serde_json::to_string(&caller.epics())?;
    let project = caller.project_id().map(|id| id.to_string());
    let mut statement = store.conn.prepare(&format!(
        "SELECT {TOPOLOGY_COLUMNS} FROM topologies WHERE archived_at IS NULL AND name>?1 \
           AND (?2 IS NULL OR owner_kind=?2) AND (?3 IS NULL OR epic_id=?3) \
           AND ((owner_kind='epic' AND epic_id IN (SELECT value FROM json_each(?4)) AND project_id IS ?5) \
             OR (owner_kind='manager' AND project_id IS ?5 AND (?6=1 OR shared=1)) \
             OR (owner_kind='operator' AND shared=1)) \
         ORDER BY name LIMIT ?7"
    ))?;
    let is_manager = i64::from(matches!(caller, TopologyCaller::Manager { .. }));
    let raw = statement
        .query_map(
            params![
                request.cursor.clone().unwrap_or_default(),
                request.scope.map(AgentTopologyScopeV1::owner_kind),
                request.epic_id.map(|id| id.to_string()),
                epics,
                project,
                is_manager,
                i64::from(limit) + 1,
            ],
            raw_topology,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut records = raw
        .into_iter()
        .map(TopologyRecord::from_raw)
        .collect::<Result<Vec<_>>>()?;
    records.retain(|record| caller.sees(record));
    let next_cursor = (records.len() > limit as usize).then(|| {
        records.truncate(limit as usize);
        records.last().map(|record| record.name.clone())
    });
    let executions = if request.include_executions {
        let epics = request
            .epic_id
            .map_or_else(|| caller.epics(), |epic| vec![epic]);
        let mut statement = store.conn.prepare(
            "SELECT id FROM topology_executions \
             WHERE status IN ('accepted','running','cancelling','blocked') \
               AND COALESCE(epic_id,parent_session_id) IN (SELECT value FROM json_each(?1)) \
             ORDER BY created_at DESC LIMIT ?2",
        )?;
        let ids = statement
            .query_map(
                params![serde_json::to_string(&epics)?, EXECUTIONS_LIST_MAX],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut summaries = Vec::with_capacity(ids.len());
        for id in ids {
            let id = Uuid::parse_str(&id)
                .map_err(|_| DaemonError::Store("invalid topology UUID".into()))?;
            if let Some(summary) = execution_summary(&store.conn, id)? {
                summaries.push(summary);
            }
        }
        summaries
    } else {
        Vec::new()
    };
    Ok(AgentTopologyListResultV1 {
        topologies: records.iter().map(TopologyRecord::summary).collect(),
        executions,
        next_cursor: next_cursor.flatten(),
    })
}

fn execution_summary(
    conn: &Connection,
    execution_id: Uuid,
) -> Result<Option<AgentTopologyExecutionSummaryV1>> {
    type Raw = (
        Option<String>,
        Option<i64>,
        String,
        Option<String>,
        Option<String>,
        String,
        i64,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
    );
    let raw: Option<Raw> = conn
        .query_row(
            "SELECT topology_id,topology_revision,topology_name_snapshot,epic_id,parent_session_id,status,\
                row_version,requested_by_kind,base_commit,blocked_reason_json,error,created_at,finished_at \
             FROM topology_executions WHERE id=?1",
            [execution_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                ))
            },
        )
        .optional()?;
    let Some((
        topology_id,
        topology_revision,
        topology_name,
        epic_id,
        parent,
        status,
        row_version,
        requested_by_kind,
        base_commit,
        blocked_reason,
        error,
        created_at,
        finished_at,
    )) = raw
    else {
        return Ok(None);
    };
    Ok(Some(AgentTopologyExecutionSummaryV1 {
        execution_id,
        topology_id: uuid_column(topology_id)?,
        topology_revision,
        topology_name,
        epic_id: uuid_column(epic_id.or(parent))?,
        status,
        row_version,
        requested_by_kind,
        base_commit,
        blocked_reason: blocked_reason
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
        error: error.map(clip),
        created_at,
        finished_at,
    }))
}

/// `AgentTopologyGetExecution` (plan §5.1).
pub(crate) fn get_execution(
    store: &Store,
    caller: Uuid,
    request: &AgentTopologyGetExecutionRequestV1,
) -> Result<AgentTopologyGetExecutionResultV1> {
    let limit = request.validated_limit().map_err(invalid)?;
    let project = execution_project(store, request.execution_id, request.project_id)?;
    let caller = TopologyCaller::resolve_in(store, caller, project).map_err(|error| {
        if request.project_id.is_none() {
            derived_project_denial(error)
        } else {
            error
        }
    })?;
    caller.require_execution(store, request.execution_id, Reach::Read)?;
    let execution = execution_summary(&store.conn, request.execution_id)?.ok_or_else(not_found)?;
    let nodes = rows::load_attempts(store, request.execution_id)?
        .iter()
        .map(crate::topology::resolve::summary)
        .collect();
    let after = request.after_sequence.unwrap_or(0);
    let mut statement = store.conn.prepare(
        "SELECT execution_seq,kind,node_id,attempt_id,actor_kind,actor_session_id,created_at \
         FROM topology_events WHERE execution_id=?1 AND execution_seq>?2 \
         ORDER BY execution_seq LIMIT ?3",
    )?;
    let events = statement
        .query_map(
            params![
                request.execution_id.to_string(),
                i64::try_from(after).unwrap_or(i64::MAX),
                i64::from(limit)
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(
            |(sequence, kind, node_id, attempt, actor_kind, actor, created_at)| {
                Ok(AgentTopologyEventV1 {
                    sequence: u64::try_from(sequence).unwrap_or_default(),
                    kind,
                    node_id,
                    attempt_id: uuid_column(attempt)?,
                    actor_kind,
                    actor_session_id: uuid_column(actor)?,
                    created_at,
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;
    let next_sequence = events.last().map_or(after, |event| event.sequence);
    Ok(AgentTopologyGetExecutionResultV1 {
        execution,
        nodes,
        events,
        next_sequence,
    })
}

/// Result of an accepted (or replayed) agent execution.
#[derive(Debug)]
pub(crate) struct Executed {
    pub(crate) result: AgentTopologyExecuteResultV1,
    /// The `accepted` update to publish; `None` for a replay.
    pub(crate) accepted: Option<GraphExecutionUpdate>,
    pub(crate) topology: Option<Topology>,
    pub(crate) project_id: Option<Uuid>,
}

fn execute_fingerprint(request: &AgentTopologyExecuteRequestV1) -> String {
    rows::digest(
        &json!({
            "topology_id": request.topology_id,
            "expected_digest": request.expected_digest,
            "epic_id": request.epic_id,
            "inputs": request.inputs,
            "base_commit": request.base_commit,
        })
        .to_string(),
    )
}

fn replayed(
    prior: &rows::PriorAcceptance,
    fingerprint: &str,
) -> Result<AgentTopologyExecuteResultV1> {
    if prior.fingerprint.as_deref() != Some(fingerprint) {
        return Err(refusal(
            "idempotency_conflict",
            "retry the original request or use a new idempotency_key",
        ));
    }
    Ok(AgentTopologyExecuteResultV1 {
        execution_id: prior.execution_id,
        accepted_at: prior.accepted_at,
        base_commit: prior.base_commit.clone(),
        deduplicated: true,
    })
}

/// Everything decided under one store lock before the Git work.
struct Admitted {
    record: TopologyRecord,
    workflow: WorkflowDefinition,
    repo: std::path::PathBuf,
    project_id: Option<Uuid>,
    policy: Option<ManagerPolicyV2>,
    scope_version: Option<i64>,
}

/// The complete mutable agent admission policy. Both sides of the Git await
/// call this under the store guard; the accept side receives fresh operator
/// knobs and resolves the current manager grant again through `caller`.
fn admission_policy(
    store: &Store,
    knobs: AgentKnobs,
    caller: &TopologyCaller,
    record: &TopologyRecord,
    epic_id: Uuid,
) -> Result<(Checked, Option<ManagerPolicyV2>, Option<i64>)> {
    caller.require_epic(store, epic_id, Reach::Effect)?;
    if !knobs.executor_enabled {
        return Err(refusal(
            "executor_disabled",
            "the operator turned off topology_executor_enabled; retry after it is re-enabled",
        ));
    }
    let policy = caller.policy(store)?;
    let allowed = policy
        .as_ref()
        .map(|policy| policy.allowed_launches.clone())
        .unwrap_or_default();
    let checked = check_definition(
        &record.topology(),
        &allowed,
        knobs.bulk_fanout_min_openrouter,
    );
    if let Some(refused) = checked.refusal() {
        return Err(refused);
    }
    // The executor limits agent session reservations to this fixed bound;
    // admission checks that the bound is usable for session-bearing flows.
    if checked.session_nodes > 0 && AGENT_MAX_PARALLEL_NODES == 0 {
        return Err(refusal(
            "policy_refused",
            "ask the operator to restore the agent parallel-node limit",
        ));
    }
    let scope_version = if let TopologyCaller::Manager { authority, .. } = caller {
        let usage = store.manager_v2_created_usage(&authority.config, false)?;
        let needed = i64::try_from(checked.session_nodes).unwrap_or(i64::MAX);
        if usage.saturating_add(needed) > i64::from(authority.grant.policy.max_created_sessions) {
            return Err(refusal(
                "creation_limit",
                "ask the operator to raise max_created_sessions for this manager",
            ));
        }
        match ancestor_allowance(store, &authority.config, needed, 0)? {
            None => {}
            Some("creation_limit") => {
                return Err(refusal(
                    "creation_limit",
                    "ask the operator to raise max_created_sessions for this manager",
                ));
            }
            Some(code) => {
                return Err(refusal(
                    code,
                    "a manager above you has no session allowance left in this project; report up",
                ));
            }
        }
        Some(authority.config.row_version)
    } else {
        None
    };
    Ok((checked, policy, scope_version))
}

fn admit(
    store: &Store,
    knobs: AgentKnobs,
    caller: &TopologyCaller,
    request: &AgentTopologyExecuteRequestV1,
) -> Result<Admitted> {
    caller.require_epic(store, request.epic_id, Reach::Effect)?;
    let record = TopologyRecord::load(&store.conn, request.topology_id)?
        .filter(|record| !record.archived && caller.may_execute(record, request.epic_id))
        .ok_or_else(not_found)?;
    if record.digest() != request.expected_digest {
        return Err(refusal(
            "digest_mismatch",
            "list the topology and retry with its current definition_digest",
        ));
    }
    let (checked, policy, scope_version) =
        admission_policy(store, knobs, caller, &record, request.epic_id)?;
    let epic = store.get_session(request.epic_id)?.ok_or_else(not_found)?;
    let project_id = epic.project_id;
    let repo = project_id
        .map(|project| store.get_project(project))
        .transpose()?
        .flatten()
        .and_then(|project| project.path)
        .ok_or_else(|| {
            refusal(
                "project_path_required",
                "the operator must give the Epic's project a repository path",
            )
        })?;
    Ok(Admitted {
        record,
        workflow: checked.workflow.ok_or_else(not_found)?,
        repo,
        project_id,
        policy,
        scope_version,
    })
}

/// `AgentTopologyExecute` (plan §5.1): accept one durable execution of a
/// visible topology under an in-scope Epic. The caller publishes the
/// `accepted` update and wakes the driver.
pub(crate) async fn execute(
    store: &Arc<Mutex<Store>>,
    knobs: impl Fn() -> AgentKnobs,
    caller_id: Uuid,
    request: &AgentTopologyExecuteRequestV1,
) -> Result<Executed> {
    let record = |store: &Store, outcome: &Result<Executed>| {
        let audit = CallAudit {
            verb: "execute",
            caller: caller_id,
            request_digest: request_digest(request),
            epic_id: Some(request.epic_id),
            topology_id: Some(request.topology_id),
            execution_id: outcome.as_ref().ok().map(|done| done.result.execution_id),
        };
        record_result(
            store,
            &audit,
            outcome.as_ref().map(|done| {
                (
                    serde_json::to_value(&done.result).ok(),
                    done.result.deduplicated,
                )
            }),
        );
    };
    let outcome = match prepare_execute(store, knobs(), caller_id, request).await {
        Ok(Prepared::Replay(executed)) => Ok(*executed),
        Ok(Prepared::Ready(prepared)) => {
            // Accepted and recorded under one guard.
            let store = store.lock().await;
            let outcome = accept_prepared(&store, knobs(), caller_id, request, *prepared);
            record(&store, &outcome);
            return outcome;
        }
        Err(error) => Err(error),
    };
    record(&*store.lock().await, &outcome);
    outcome
}

/// An execute request decided before or after its Git work.
pub(crate) enum Prepared {
    /// The idempotency key replayed an earlier acceptance.
    Replay(Box<Executed>),
    /// Admitted and custody-resolved; not yet persisted.
    Ready(Box<PreparedExecution>),
}

/// Everything resolved before the accepting lock.
pub(crate) struct PreparedExecution {
    admitted: Admitted,
    fingerprint: String,
    repo_root: std::path::PathBuf,
    base_commit: String,
    custody_plan: crate::topology::custody::TopologyCustodyPlan,
}

/// Admission under one lock, then the unlocked Git work (`origin/rolling`
/// base resolution). Nothing is persisted; [`accept_prepared`] rechecks.
pub(crate) async fn prepare_execute(
    store: &Arc<Mutex<Store>>,
    knobs: AgentKnobs,
    caller_id: Uuid,
    request: &AgentTopologyExecuteRequestV1,
) -> Result<Prepared> {
    request.validate().map_err(invalid)?;
    let fingerprint = execute_fingerprint(request);
    let admitted = {
        let store = store.lock().await;
        let caller = TopologyCaller::resolve_in(&store, caller_id, request.project_id)?;
        // Authorize first, replay second (review R2): a replay passes the
        // same live authority and Epic scope a fresh request needs, plus the
        // prior execution's own scope.
        caller.require_epic(&store, request.epic_id, Reach::Effect)?;
        if let Some(prior) =
            rows::prior_acceptance(&store.conn, caller_id, &request.idempotency_key)?
        {
            let replay = replayed(&prior, &fingerprint)?;
            authorize_execution(&store, caller_id, replay.execution_id, Reach::Effect)?;
            return Ok(Prepared::Replay(Box::new(Executed {
                result: replay,
                accepted: None,
                topology: None,
                project_id: None,
            })));
        }
        admit(&store, knobs, &caller, request)?
    };
    let steps = crate::session::graph_executions::validate_live_workflow(&admitted.workflow)?;
    let (repo_root, base_commit) = crate::session::graph_executions::resolve_live_custody_base(
        store,
        &admitted.workflow,
        &steps,
        &admitted.repo,
        request.base_commit.as_deref(),
    )
    .await
    .map_err(|error| {
        tracing::warn!(%error, "agent topology execution base refused");
        refusal(
            "execution_base_refused",
            "check base_commit and that the Epic's project repository is a clean checkout, then retry",
        )
    })?;
    let custody_plan = crate::session::graph_runner::plan_workflow_custody(&admitted.workflow)?;
    Ok(Prepared::Ready(Box::new(PreparedExecution {
        admitted,
        fingerprint,
        repo_root,
        base_commit,
        custody_plan,
    })))
}

/// Persist a prepared execution under the accepting lock. Authority, scope
/// and the topology itself are re-read here: an unshare, archive or edit
/// during the Git work refuses and persists nothing.
pub(crate) fn accept_prepared(
    store: &Store,
    knobs: AgentKnobs,
    caller_id: Uuid,
    request: &AgentTopologyExecuteRequestV1,
    prepared: PreparedExecution,
) -> Result<Executed> {
    let PreparedExecution {
        admitted,
        fingerprint,
        repo_root,
        base_commit,
        custody_plan,
    } = prepared;
    let caller = TopologyCaller::resolve_in(store, caller_id, request.project_id)?;
    caller.require_epic(store, request.epic_id, Reach::Effect)?;
    let current = TopologyRecord::load(&store.conn, admitted.record.id)?
        .filter(|record| !record.archived && caller.may_execute(record, request.epic_id))
        .ok_or_else(|| {
            refusal(
                "topology_not_visible",
                "the topology is no longer visible to you; list your scope and retry",
            )
        })?;
    if current.revision != admitted.record.revision
        || current.digest() != admitted.record.digest()
        || current.digest() != request.expected_digest
    {
        return Err(refusal(
            "topology_changed",
            "the topology changed while the execution was prepared; list it and retry with its current definition_digest",
        ));
    }
    // Re-run the entire admission path with current policy and settings. A
    // changed grant, knob, project path or budget refuses before any row.
    let fresh = admit(store, knobs, &caller, request)?;
    if fresh.project_id != admitted.project_id || fresh.repo != admitted.repo {
        return Err(refusal(
            "project_path_changed",
            "the Epic's project repository changed while preparing; retry the execution",
        ));
    }
    let actor = caller.actor();
    let new = NewExecution {
        id: Uuid::new_v4(),
        topology_id: Some(admitted.record.id),
        workflow_id: Uuid::new_v4(),
        definition: admitted.workflow,
        custody_plan,
        project_id: admitted.project_id,
        parent_session_id: Some(request.epic_id),
        repo_root,
        base_commit,
        input: (!request.inputs.is_null()).then(|| request.inputs.clone()),
        requester: Some(ExecutionRequester {
            actor,
            epic_id: request.epic_id,
            scope_version: fresh.scope_version,
            policy_digest: fresh
                .policy
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?
                .map(|text| rows::digest(&text)),
            idempotency_key: request.idempotency_key.clone(),
            request_fingerprint: fingerprint,
            topology_revision: Some(admitted.record.revision),
        }),
    };
    let base_commit = new.base_commit.clone();
    let id = new.id;
    match rows::accept_agent_execution(store, &new)? {
        Acceptance::Fresh(update) => Ok(Executed {
            result: AgentTopologyExecuteResultV1 {
                execution_id: id,
                accepted_at: update.updated_at,
                base_commit,
                deduplicated: false,
            },
            accepted: Some(update),
            topology: Some(admitted.record.topology()),
            project_id: admitted.project_id,
        }),
        Acceptance::Replay {
            execution_id,
            accepted_at,
            base_commit,
        } => Ok(Executed {
            result: AgentTopologyExecuteResultV1 {
                execution_id,
                accepted_at,
                base_commit,
                deduplicated: true,
            },
            accepted: None,
            topology: None,
            project_id: None,
        }),
    }
}

/// `AgentTopologyInterrupt` (plan §5.1). Returns the result and the update
/// to publish (`None` for a replay); the caller wakes the driver.
pub(crate) fn interrupt(
    store: &Store,
    caller: Uuid,
    request: &AgentTopologyInterruptRequestV1,
) -> Result<(AgentTopologyInterruptResultV1, Vec<GraphExecutionUpdate>)> {
    let outcome = interrupt_authorized(store, caller, request);
    record_result(
        store,
        &CallAudit {
            verb: "interrupt",
            caller,
            request_digest: request_digest(request),
            epic_id: None,
            topology_id: None,
            execution_id: Some(request.execution_id),
        },
        outcome
            .as_ref()
            .map(|(result, _)| (serde_json::to_value(result).ok(), result.deduplicated)),
    );
    outcome
}

fn interrupt_authorized(
    store: &Store,
    caller: Uuid,
    request: &AgentTopologyInterruptRequestV1,
) -> Result<(AgentTopologyInterruptResultV1, Vec<GraphExecutionUpdate>)> {
    request.validate().map_err(invalid)?;
    execution_project(store, request.execution_id, request.project_id)?;
    let (caller, _) = authorize_execution(store, caller, request.execution_id, Reach::Stop)?;
    let requested_at = Utc::now();
    let outcome = rows::request_agent_interrupt(
        store,
        request.execution_id,
        request.expected_row_version,
        &request.idempotency_key,
        caller.actor(),
    );
    let (recorded, status) = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            audit_refusal(store, request.execution_id, &caller, "interrupt", &error);
            return Err(error);
        }
    };
    let (updates, deduplicated) = match recorded {
        Recorded::Fresh(updates) => (updates, false),
        Recorded::Replay => (Vec::new(), true),
    };
    Ok((
        AgentTopologyInterruptResultV1 {
            execution_id: request.execution_id,
            status: status.wire(),
            interrupt_requested_at: requested_at,
            deduplicated,
        },
        updates,
    ))
}

/// Durable audit of a refused request against an execution the caller may
/// see (plan §5.3). Best effort: the refusal itself is returned regardless.
fn audit_refusal(
    store: &Store,
    execution_id: Uuid,
    caller: &TopologyCaller,
    verb: &str,
    error: &DaemonError,
) {
    // A valid caller identity alone never authorizes an event on this
    // execution. Foreign requests still receive their one request-ledger row.
    if authorize_execution(store, caller.session(), execution_id, Reach::Read).is_err() {
        return;
    }
    let code = match error {
        DaemonError::StructuredRpc { data, .. } => data
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("request_failed")
            .to_owned(),
        _ => "request_failed".to_owned(),
    };
    if let Err(audit) = rows::record_agent_refusal(store, execution_id, caller.actor(), verb, &code)
    {
        tracing::warn!(%audit, %execution_id, "agent topology refusal audit failed");
    }
}

/// `AgentTopologyResolveAttempt` (plan §3.4, §5.1): the operator resolution
/// function, bound to the caller's scope. A lead cannot discard preserved
/// bytes; the manager needs `Automation` (already required to resolve).
pub(crate) async fn resolve<E: NodeEffects>(
    executor: &Executor<E>,
    caller_id: Uuid,
    params: &ResolveTopologyAttemptParams,
) -> Result<ResolveTopologyAttemptResponse> {
    // Initial check, then the same check again under every guard that
    // decides or records the resolution (review R2).
    let authorize = |store: &Store| authorize_resolution(store, caller_id, params);
    let under_lock = |store: &Store| authorize(store).map(|caller| caller.actor());
    let initial = {
        let store = executor.store.lock().await;
        authorize(&store).map(drop)
    };
    let outcome = match initial {
        Ok(()) => executor.resolve_attempt_as(params, &under_lock).await,
        Err(error) => Err(error),
    };
    let store = executor.store.lock().await;
    if let Err(error) = &outcome
        && let Ok(caller) = TopologyCaller::resolve(&store, caller_id)
    {
        audit_refusal(
            &store,
            params.execution_id,
            &caller,
            "resolve_attempt",
            error,
        );
    }
    record_result(
        &store,
        &CallAudit {
            verb: "resolve_attempt",
            caller: caller_id,
            request_digest: request_digest(params),
            epic_id: None,
            topology_id: None,
            execution_id: Some(params.execution_id),
        },
        outcome
            .as_ref()
            .map(|response| (serde_json::to_value(response).ok(), response.deduplicated)),
    );
    drop(store);
    outcome
}

/// A project the daemon derived from an execution (not one the caller named)
/// keeps the ordinary `authority_denied` for a caller outside it.
fn derived_project_denial(error: DaemonError) -> DaemonError {
    if error_code(&error) == rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE {
        denied()
    } else {
        error
    }
}

/// The project of an execution's Epic (#1235). A named `project_id` that
/// differs is refused `manager_project_not_in_scope`; an unknown execution
/// resolves to `None` so the ordinary not-found path answers.
fn execution_project(
    store: &Store,
    execution_id: Uuid,
    named: Option<Uuid>,
) -> Result<Option<Uuid>> {
    let project = rows::load_execution(store, execution_id)?
        .and_then(|execution| execution.epic_id.or(execution.parent_session_id))
        .map(|epic| store.get_session(epic))
        .transpose()?
        .flatten()
        .and_then(|epic| epic.project_id);
    if named.is_some_and(|named| Some(named) != project) {
        return Err(refusal(
            rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE,
            "name the project that owns the execution, or omit project_id",
        ));
    }
    Ok(project)
}

/// Live authority of `caller_id` over `execution_id` at `reach`, read from
/// durable rows under the caller's store guard: current manager appointment
/// with `Automation`, the execution's Epic in live scope (plus Execute mode
/// and no pause for effects), or the current lead of that Epic. Shared by
/// every T4 mutation that authorizes, awaits and then writes.
pub(crate) fn authorize_execution(
    store: &Store,
    caller_id: Uuid,
    execution_id: Uuid,
    reach: Reach,
) -> Result<(TopologyCaller, ExecutionRow)> {
    // #1235: an execution-keyed verb acts in the execution's own project.
    let project = execution_project(store, execution_id, None)?;
    let caller =
        TopologyCaller::resolve_in(store, caller_id, project).map_err(derived_project_denial)?;
    let execution = caller.require_execution(store, execution_id, reach)?;
    Ok((caller, execution))
}

/// [`authorize_execution`] for one resolution action; a lead never
/// discards preserved bytes.
fn authorize_resolution(
    store: &Store,
    caller_id: Uuid,
    params: &ResolveTopologyAttemptParams,
) -> Result<TopologyCaller> {
    let reach = if params.action == TopologyAttemptAction::Inspect {
        Reach::Read
    } else {
        Reach::Effect
    };
    let (caller, _) = authorize_execution(store, caller_id, params.execution_id, reach)?;
    if params.action == TopologyAttemptAction::Discard
        && matches!(caller, TopologyCaller::Lead { .. })
    {
        return Err(refusal(
            "discard_requires_manager",
            "ask the manager (Automation) or the operator to discard preserved work",
        ));
    }
    Ok(caller)
}
