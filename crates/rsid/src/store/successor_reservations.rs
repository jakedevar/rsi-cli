//! Durable master-successor reservations and the atomic Epic baton transfer.

use super::row_mappers::{
    SESSION_COLUMNS, map_session_row, session_kind_to_str, session_provider_to_str,
};
use super::sandbox_custody::{SessionCustodyBinding, bind_on, lock_custody_root};
use super::sessions::{
    OwningEpicTopologyError, insert_session_on, resolve_owning_epic_topology_tx,
};
use super::{Store, parse_timestamp};
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_coordination::{
    AgentReserveSuccessorRequestV1, AgentReserveSuccessorResultV1, AgentSuccessorStateV1,
};
use rsi_common::types::{Session, SessionKind, SessionStatus, is_leaf_kind, legal_children};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

pub(crate) const AGENT_SUCCESSOR_RECONCILE_PAGE_MAX: usize = 256;

mod unpublished;
pub(crate) use unpublished::{
    UNPUBLISHED_SUCCESSOR_ADMISSION_DENIED, UNPUBLISHED_SUCCESSOR_CLEANUP,
    UnpublishedSuccessorCleanupClaim,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentSuccessorReservationIds {
    pub reservation_id: Uuid,
    pub candidate_session_id: Uuid,
    pub transition_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentSuccessorLaunchIds {
    pub launch_attempt_id: Uuid,
    pub model_invocation_id: Uuid,
    pub transition_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentSuccessorReservation {
    pub reservation_id: Uuid,
    pub predecessor_session_id: Uuid,
    pub epic_id: Uuid,
    pub candidate_session_id: Uuid,
    pub caller_key_digest: String,
    pub request: AgentReserveSuccessorRequestV1,
    pub request_fingerprint: String,
    pub candidate_kind: SessionKind,
    pub inherited_launch_json: String,
    pub expected_lead_session_id: Uuid,
    pub expected_lead_generation: u64,
    pub state: AgentSuccessorStateV1,
    pub state_version: u64,
    pub launch_attempt_id: Option<Uuid>,
    pub model_invocation_id: Option<Uuid>,
    pub establishment_evidence_json: Option<String>,
    pub establishment_digest: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub terminal_reason: Option<String>,
    pub safe_error_class: Option<String>,
    pub reserved_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl AgentSuccessorReservation {
    #[must_use]
    pub(crate) fn receipt(&self, deduplicated: bool) -> AgentReserveSuccessorResultV1 {
        AgentReserveSuccessorResultV1 {
            reservation_id: self.reservation_id,
            predecessor_session_id: self.predecessor_session_id,
            epic_id: self.epic_id,
            candidate_session_id: self.candidate_session_id,
            kind: self.candidate_kind,
            state: self.state,
            state_version: self.state_version,
            deduplicated,
            safe_error_class: self.safe_error_class.clone(),
        }
    }

    pub(crate) fn inherited_launch(&self) -> Result<InheritedLaunchWitness> {
        serde_json::from_str(&self.inherited_launch_json).map_err(|error| {
            DaemonError::Store(format!(
                "agent successor frozen launch witness is invalid: {error}"
            ))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReserveAgentSuccessorOutcome {
    Reserved(AgentSuccessorReservation),
    Replayed(AgentSuccessorReservation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaimAgentSuccessorOutcome {
    Claimed(AgentSuccessorReservation),
    NotReady(AgentSuccessorReservation),
    Stale(AgentSuccessorReservation),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentSuccessorCursor {
    pub updated_at: DateTime<Utc>,
    pub reservation_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentSuccessorPage {
    pub reservations: Vec<AgentSuccessorReservation>,
    pub next_cursor: Option<AgentSuccessorCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentSuccessorCommitFault {
    AfterAuditInsert,
    AfterCandidateValidation,
    AfterLeadCas,
    AfterAggregateCas,
    BeforeCommit,
}

#[cfg(test)]
thread_local! {
    static COMMIT_FAULT: std::cell::RefCell<Option<AgentSuccessorCommitFault>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn successor_test_fail_next_commit(fault: AgentSuccessorCommitFault) {
    COMMIT_FAULT.with(|slot| *slot.borrow_mut() = Some(fault));
}

#[cfg(test)]
fn commit_fault(fault: AgentSuccessorCommitFault) -> Result<()> {
    if COMMIT_FAULT.with(|slot| {
        if *slot.borrow() == Some(fault) {
            *slot.borrow_mut() = None;
            true
        } else {
            false
        }
    }) {
        return Err(DaemonError::Store(format!(
            "injected successor authority commit fault: {fault:?}"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn commit_fault(_: AgentSuccessorCommitFault) -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InheritedLaunchWitness {
    provider: String,
    pub(crate) working_dir: String,
    pub(crate) project_id: Option<Uuid>,
    pub(crate) sandbox_kind: Option<String>,
    pub(crate) sandbox_root: Option<String>,
    pub(crate) sandbox_branch: Option<String>,
    pub(crate) sandbox_cleanup_state: Option<String>,
    pub(crate) workflow_id: Option<Uuid>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) effort: Option<String>,
    #[serde(default)]
    pub(crate) agent_role: Option<String>,
    #[serde(default)]
    pub(crate) epic_spawn_ordinal: Option<u32>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    pub(crate) is_eval: bool,
    #[serde(default)]
    pub(crate) rotation_disabled_at: Option<DateTime<Utc>>,
}

impl InheritedLaunchWitness {
    pub(crate) fn provider(&self) -> Result<rsi_common::types::SessionProvider> {
        match self.provider.as_str() {
            "Claude" => Ok(rsi_common::types::SessionProvider::Claude),
            "Codex" => Ok(rsi_common::types::SessionProvider::Codex),
            "Pioneer" => Ok(rsi_common::types::SessionProvider::Pioneer),
            "OpenRouter" => Ok(rsi_common::types::SessionProvider::OpenRouter),
            "Bedrock" => Ok(rsi_common::types::SessionProvider::Bedrock),
            "Local" => Ok(rsi_common::types::SessionProvider::Local),
            "Antigravity" => Ok(rsi_common::types::SessionProvider::Antigravity),
            "CodexAppServer" => Ok(rsi_common::types::SessionProvider::CodexAppServer),
            "Harness" => Ok(rsi_common::types::SessionProvider::Harness),
            _ => Err(DaemonError::Store(
                "agent successor frozen provider is invalid".into(),
            )),
        }
    }
}

const RESERVATION_SELECT: &str = "reservation_id,predecessor_session_id,epic_id,candidate_session_id,caller_key_digest,request_json,request_fingerprint,candidate_kind,inherited_launch_json,expected_lead_session_id,expected_lead_generation,state,state_version,launch_attempt_id,model_invocation_id,establishment_evidence_json,establishment_digest,published_at,terminal_reason,safe_error_class,reserved_at,updated_at";

fn digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_uuid(raw: String, field: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(&raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid {field}: {error}"),
            )),
        )
    })
}

fn parse_optional_uuid(raw: Option<String>, field: &str) -> rusqlite::Result<Option<Uuid>> {
    raw.map(|value| parse_uuid(value, field)).transpose()
}

fn map_reservation(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentSuccessorReservation> {
    let request_json: String = row.get(5)?;
    let request = serde_json::from_str(&request_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let candidate_kind_raw: String = row.get(7)?;
    let candidate_kind =
        super::row_mappers::str_to_session_kind(&candidate_kind_raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    error.to_string(),
                )),
            )
        })?;
    let state_raw: String = row.get(11)?;
    let state = AgentSuccessorStateV1::from_str_exact(&state_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            11,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid successor state: {state_raw}"),
            )),
        )
    })?;
    let timestamp = |column: usize, raw: String| {
        parse_timestamp(&raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
            )
        })
    };
    let published_raw: Option<String> = row.get(17)?;
    Ok(AgentSuccessorReservation {
        reservation_id: parse_uuid(row.get(0)?, "reservation_id")?,
        predecessor_session_id: parse_uuid(row.get(1)?, "predecessor_session_id")?,
        epic_id: parse_uuid(row.get(2)?, "epic_id")?,
        candidate_session_id: parse_uuid(row.get(3)?, "candidate_session_id")?,
        caller_key_digest: row.get(4)?,
        request,
        request_fingerprint: row.get(6)?,
        candidate_kind,
        inherited_launch_json: row.get(8)?,
        expected_lead_session_id: parse_uuid(row.get(9)?, "expected_lead_session_id")?,
        expected_lead_generation: u64::try_from(row.get::<_, i64>(10)?).map_err(|_| {
            rusqlite::Error::IntegralValueOutOfRange(10, row.get::<_, i64>(10).unwrap_or(-1))
        })?,
        state,
        state_version: u64::try_from(row.get::<_, i64>(12)?).map_err(|_| {
            rusqlite::Error::IntegralValueOutOfRange(12, row.get::<_, i64>(12).unwrap_or(-1))
        })?,
        launch_attempt_id: parse_optional_uuid(row.get(13)?, "launch_attempt_id")?,
        model_invocation_id: parse_optional_uuid(row.get(14)?, "model_invocation_id")?,
        establishment_evidence_json: row.get(15)?,
        establishment_digest: row.get(16)?,
        published_at: published_raw.map(|raw| timestamp(17, raw)).transpose()?,
        terminal_reason: row.get(18)?,
        safe_error_class: row.get(19)?,
        reserved_at: timestamp(20, row.get(20)?)?,
        updated_at: timestamp(21, row.get(21)?)?,
    })
}

fn get_reservation_on(
    tx: &Transaction<'_>,
    reservation_id: Uuid,
) -> Result<Option<AgentSuccessorReservation>> {
    Ok(tx
        .query_row(
            &format!(
                "SELECT {RESERVATION_SELECT} FROM agent_successor_reservations WHERE reservation_id=?1"
            ),
            [reservation_id.to_string()],
            map_reservation,
        )
        .optional()?)
}

fn get_session_on(tx: &Transaction<'_>, session_id: Uuid) -> Result<Option<Session>> {
    let row = tx
        .query_row(
            &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id=?1"),
            [session_id.to_string()],
            map_session_row,
        )
        .optional()?;
    row.map(super::row_mappers::SessionRow::into_session)
        .transpose()
}

/// Prove the complete generation-one custody aggregate required for a
/// successor provider effect or Epic-lead transfer. The public Session tuple,
/// immutable allocation event, authoritative root, SQL-only link, and cached
/// execution projection must all describe the same candidate-owned worktree.
fn agent_successor_live_custody_matches_on(
    tx: &Transaction<'_>,
    candidate_session_id: Uuid,
) -> Result<bool> {
    tx.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM sessions s
             JOIN sandbox_custody_roots r ON r.custody_id=s.sandbox_custody_id
             JOIN sandbox_custody_events e
               ON e.custody_id=r.custody_id AND e.sequence=1
             JOIN session_execution_projections p ON p.session_id=s.id
             WHERE s.id=?1
               AND s.sandbox_kind='GitWorktree'
               AND s.sandbox_cleanup_state='Live'
               AND s.working_dir=r.canonical_repo_dir
               AND s.sandbox_root=r.sandbox_root
               AND s.sandbox_branch=r.sandbox_branch
               AND r.allocation_id=s.id
               AND r.owner_session_id=s.id
               AND r.state='live'
               AND r.generation=1
               AND r.validation_state='verified'
               AND r.validated_generation=1
               AND r.validated_at IS NOT NULL
               AND r.validation_error_code IS NULL
               AND r.repository_identity<>''
               AND r.source_commit<>''
               AND e.event_kind='allocated'
               AND e.cause='fresh_launch'
               AND e.from_generation IS NULL
               AND e.to_generation=1
               AND e.from_owner_session_id IS NULL
               AND e.to_owner_session_id=s.id
               AND e.prior_state IS NULL
               AND e.next_state='live'
               AND e.error_code IS NULL
               AND p.schema_version=1
               AND p.projection_version=1
               AND p.execution_state='live_sandboxed'
               AND p.freshness='verified'
               AND p.canonical_repo_dir=r.canonical_repo_dir
               AND p.effective_cwd=r.sandbox_root
               AND p.custody_id=r.custody_id
               AND p.custody_generation=r.generation
               AND p.validated_at IS NOT NULL
               AND p.error_code IS NULL
         )",
        [candidate_session_id.to_string()],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn owning_epic(tx: &Transaction<'_>, caller: &Session) -> Result<Session> {
    resolve_owning_epic_topology_tx(tx, caller).map_err(|error| {
        let message = match error {
            OwningEpicTopologyError::MissingOwningEpic => "agent_successor_missing_owning_epic",
            OwningEpicTopologyError::Cycle => "agent_successor_hierarchy_cycle",
            OwningEpicTopologyError::MissingAncestor => {
                "agent_successor_missing_hierarchy_ancestor"
            }
            OwningEpicTopologyError::IllegalEdge => "agent_successor_illegal_hierarchy_edge",
            OwningEpicTopologyError::MultipleOwningEpics => "agent_successor_multiple_owning_epics",
            OwningEpicTopologyError::DepthExceeded => "agent_successor_hierarchy_depth_exceeded",
        };
        DaemonError::PolicyDenied(message.into())
    })
}

fn inherited_launch_json(caller: &Session) -> Result<String> {
    let working_dir = caller.working_dir.to_str().ok_or_else(|| {
        DaemonError::Store("agent successor working directory is not UTF-8".into())
    })?;
    serde_json::to_string(&InheritedLaunchWitness {
        provider: session_provider_to_str(caller.provider).to_string(),
        working_dir: working_dir.to_string(),
        project_id: caller.project_id,
        sandbox_kind: caller.sandbox_kind.map(|kind| format!("{kind:?}")),
        sandbox_root: caller
            .sandbox_root
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        sandbox_branch: caller.sandbox_branch.clone(),
        sandbox_cleanup_state: caller
            .sandbox_cleanup_state
            .map(|state| format!("{state:?}")),
        workflow_id: caller.workflow_id,
        model: caller.model.clone(),
        effort: caller.effort.clone(),
        agent_role: caller.agent_role.clone(),
        epic_spawn_ordinal: caller.epic_spawn_ordinal,
        tags: caller.tags.clone(),
        is_eval: caller.is_eval,
        rotation_disabled_at: caller.rotation_disabled_at,
    })
    .map_err(Into::into)
}

fn authority_digest(record: &AgentSuccessorReservation, next: AgentSuccessorStateV1) -> String {
    digest(&[
        "agent.successor.authority.v1",
        &record.reservation_id.to_string(),
        &record.predecessor_session_id.to_string(),
        &record.epic_id.to_string(),
        &record.expected_lead_generation.to_string(),
        next.as_str(),
    ])
}

fn insert_transition(
    tx: &Transaction<'_>,
    record: &AgentSuccessorReservation,
    transition_id: Uuid,
    from: Option<AgentSuccessorStateV1>,
    to: AgentSuccessorStateV1,
    version: u64,
    reason: Option<&str>,
    at: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO agent_successor_transitions
         (transition_id,reservation_id,state_version,from_state,to_state,authority_digest,launch_attempt_id,model_invocation_id,reason,created_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            transition_id.to_string(),
            record.reservation_id.to_string(),
            i64::try_from(version).map_err(|_| DaemonError::InvalidParam("agent successor version overflow".into()))?,
            from.map(AgentSuccessorStateV1::as_str),
            to.as_str(),
            authority_digest(record, to),
            record.launch_attempt_id.map(|id| id.to_string()),
            record.model_invocation_id.map(|id| id.to_string()),
            reason,
            at,
        ],
    )?;
    Ok(())
}

/// One predecessor, one continuation.
///
/// The successor-reservation kernel is authoritative for Epic lead lineage: a
/// predecessor with a live or committed reservation already owns, or is about
/// to own, its single `continued_from` successor. Every other turnover
/// mechanism — and every later reservation under a different idempotency key —
/// must refuse rather than give that predecessor a second successor. Two
/// receipted successors are exactly what makes `manager_lineage_tip` return
/// `manager_lineage_ambiguous`, which is load-bearing across harness-manager
/// v1.
///
/// `failed` is deliberately excluded. A settled reservation transferred no
/// authority, yet its candidate row keeps `continued_from` forever (the
/// terminal settlement re-writes it, `settle_agent_successor` below), so
/// treating it as a published continuation would permanently disable context
/// rotation for any session whose baton handoff once failed.
///
/// The refusal names the earliest reservation, matching the V111 convergence
/// rule that the lowest `expected_lead_generation` is canonical.
pub(super) fn reject_agent_successor_predecessor_continuation_on(
    connection: &Connection,
    predecessor_session_id: Uuid,
) -> Result<()> {
    let reservation_id = connection
        .query_row(
            "SELECT reservation_id FROM agent_successor_reservations
             WHERE predecessor_session_id=?1
               AND state IN ('reserved','launching','uncertain','committed')
             ORDER BY reserved_at,reservation_id LIMIT 1",
            [predecessor_session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(reservation_id) = reservation_id {
        return Err(DaemonError::PolicyDenied(format!(
            "agent_successor_predecessor_already_continued:{predecessor_session_id}:{reservation_id}"
        )));
    }
    Ok(())
}

pub(super) fn reject_nonterminal_agent_successor_epic_lead_mutation_on(
    connection: &Connection,
    epic_id: Uuid,
) -> Result<()> {
    let reservation_id = connection
        .query_row(
            "SELECT reservation_id FROM agent_successor_reservations
             WHERE epic_id=?1 AND state IN ('reserved','launching','uncertain')
             LIMIT 1",
            [epic_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(reservation_id) = reservation_id {
        return Err(DaemonError::PolicyDenied(format!(
            "agent_successor_epic_lead_locked:{epic_id}:{reservation_id}"
        )));
    }
    Ok(())
}

pub(super) fn reject_nonterminal_agent_successor_lead_clear_on(
    connection: &Connection,
    lead_session_id: Uuid,
) -> Result<()> {
    let reservation_id = connection
        .query_row(
            "SELECT r.reservation_id
             FROM agent_successor_reservations r
             JOIN sessions epic ON epic.id=r.epic_id
             WHERE epic.lead_session_id=?1
               AND r.state IN ('reserved','launching','uncertain')
             LIMIT 1",
            [lead_session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(reservation_id) = reservation_id {
        return Err(DaemonError::PolicyDenied(format!(
            "agent_successor_epic_lead_locked:{lead_session_id}:{reservation_id}"
        )));
    }
    Ok(())
}

impl Store {
    pub(crate) fn reject_nonterminal_agent_successor_candidate_topology_mutation(
        &self,
        candidate_session_id: Uuid,
    ) -> Result<()> {
        let reservation_id = self
            .conn
            .query_row(
                "SELECT reservation_id FROM agent_successor_reservations
                 WHERE (candidate_session_id=?1 OR expected_lead_session_id=?1)
                   AND state IN ('reserved','launching','uncertain')
                 LIMIT 1",
                [candidate_session_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(reservation_id) = reservation_id {
            return Err(DaemonError::PolicyDenied(format!(
                "agent_successor_candidate_topology_locked:{candidate_session_id}:{reservation_id}"
            )));
        }
        Ok(())
    }

    pub(crate) fn reject_nonterminal_agent_successor_epic_lead_mutation(
        &self,
        epic_id: Uuid,
    ) -> Result<()> {
        reject_nonterminal_agent_successor_epic_lead_mutation_on(&self.conn, epic_id)
    }

    pub(crate) fn reject_nonterminal_agent_successor_lead_clear(
        &self,
        lead_session_id: Uuid,
    ) -> Result<()> {
        reject_nonterminal_agent_successor_lead_clear_on(&self.conn, lead_session_id)
    }

    /// Persist the stable candidate, exact invocation binding, frozen tags,
    /// and its distinct generation-one custody root as one pre-provider-effect
    /// boundary. Generic launch finalization must never be responsible for any
    /// member of the successor authority witness.
    pub(crate) fn insert_agent_successor_session_with_custody(
        &self,
        reservation: &AgentSuccessorReservation,
        session: &Session,
        invocation_id: Uuid,
        binding: SessionCustodyBinding,
    ) -> Result<()> {
        let custody_id = match &binding {
            SessionCustodyBinding::New(root) => root.custody_id,
            SessionCustodyBinding::Ordinary
            | SessionCustodyBinding::Reuse { .. }
            | SessionCustodyBinding::Transfer { .. } => {
                return Err(DaemonError::Store(
                    "agent successor persistence requires a new custody root".into(),
                ));
            }
        };
        let _root_guard = lock_custody_root(custody_id);
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = get_reservation_on(&tx, reservation.reservation_id)?.ok_or_else(|| {
            DaemonError::InvalidParam(format!(
                "agent_successor_unknown:{}",
                reservation.reservation_id
            ))
        })?;
        let frozen = current.inherited_launch()?;
        let expected_tags = current
            .request
            .tags
            .clone()
            .unwrap_or_else(|| frozen.tags.clone());
        if current.state != reservation.state
            || current.state_version != reservation.state_version
            || !matches!(
                current.state,
                AgentSuccessorStateV1::Launching | AgentSuccessorStateV1::Uncertain
            )
            || current.launch_attempt_id.is_none()
            || current.launch_attempt_id != reservation.launch_attempt_id
            || current.model_invocation_id != Some(invocation_id)
            || current.model_invocation_id != reservation.model_invocation_id
            || session.id != current.candidate_session_id
            || session.session_kind != current.candidate_kind
            || session.parent_id != Some(current.epic_id)
            || session.continued_from != Some(current.predecessor_session_id)
            || session.provider != frozen.provider()?
            || session.project_id != frozen.project_id
            || session.working_dir.as_path() != std::path::Path::new(&frozen.working_dir)
            || session.query != current.request.query
            || session.model != current.request.model.clone().or(frozen.model)
            || session.effort != current.request.effort.clone().or(frozen.effort)
            || session.tags != expected_tags
            || session.topology_node_id != current.request.topology_node
            || session.topology_iteration != current.request.iteration.unwrap_or(0)
            || session.status != SessionStatus::Starting
            || (frozen.rotation_disabled_at.is_some()
                && session.rotation_disabled_at != frozen.rotation_disabled_at)
        {
            return Err(DaemonError::InvalidParam(
                "agent_successor_pre_effect_binding_mismatch".into(),
            ));
        }
        let invocation_matches: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM model_invocations
             WHERE id=?1 AND session_id=?2 AND purpose='agent.reserve_successor'
               AND admission_status='admitted' AND status='running')",
            params![invocation_id.to_string(), session.id.to_string()],
            |row| row.get(0),
        )?;
        if !invocation_matches {
            return Err(DaemonError::InvalidParam(
                "agent_successor_pre_effect_invocation_mismatch".into(),
            ));
        }
        insert_session_on(&tx, session)?;
        let bound = tx.execute(
            "UPDATE sessions SET model_invocation_id=?1,updated_at=?2 WHERE id=?3",
            params![
                invocation_id.to_string(),
                now_string(),
                session.id.to_string()
            ],
        )?;
        if bound != 1 {
            return Err(DaemonError::Store(
                "agent successor candidate invocation binding failed".into(),
            ));
        }
        for tag in &session.tags {
            tx.execute(
                "INSERT OR IGNORE INTO session_tags(session_id,tag) VALUES(?1,?2)",
                params![session.id.to_string(), tag],
            )?;
        }
        bind_on(&tx, session.id, binding)?;
        tx.commit()?;
        Ok(())
    }

    /// Freshly prove the exact durable baton witness immediately before a
    /// provider effect. The transaction covers the reservation aggregate, the
    /// candidate incarnation, its admitted invocation, and the predecessor's
    /// still-exclusive Epic lead generation.
    pub(crate) fn validate_agent_successor_provider_effect_fence(
        &self,
        expected: &AgentSuccessorReservation,
        invocation_id: Uuid,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = get_reservation_on(&tx, expected.reservation_id)?.ok_or_else(|| {
            DaemonError::PolicyDenied(format!(
                "agent_successor_provider_effect_reservation_missing:{}",
                expected.reservation_id
            ))
        })?;
        if current.predecessor_session_id != expected.predecessor_session_id
            || current.epic_id != expected.epic_id
            || current.candidate_session_id != expected.candidate_session_id
            || current.caller_key_digest != expected.caller_key_digest
            || current.request_fingerprint != expected.request_fingerprint
            || current.candidate_kind != expected.candidate_kind
            || current.inherited_launch_json != expected.inherited_launch_json
            || current.expected_lead_session_id != expected.expected_lead_session_id
            || current.expected_lead_generation != expected.expected_lead_generation
            || current.state != expected.state
            || !matches!(
                current.state,
                AgentSuccessorStateV1::Launching | AgentSuccessorStateV1::Uncertain
            )
            || current.state_version != expected.state_version
            || current.launch_attempt_id != expected.launch_attempt_id
            || current.model_invocation_id != Some(invocation_id)
            || current.model_invocation_id != expected.model_invocation_id
        {
            return Err(DaemonError::PolicyDenied(format!(
                "agent_successor_provider_effect_reservation_mismatch:{}",
                expected.reservation_id
            )));
        }

        let frozen = current.inherited_launch()?;
        let mut expected_tags = current
            .request
            .tags
            .clone()
            .unwrap_or_else(|| frozen.tags.clone());
        let candidate = get_session_on(&tx, current.candidate_session_id)?.ok_or_else(|| {
            DaemonError::PolicyDenied(format!(
                "agent_successor_provider_effect_candidate_missing:{}",
                current.candidate_session_id
            ))
        })?;
        let candidate_invocation: Option<String> = tx.query_row(
            "SELECT model_invocation_id FROM sessions WHERE id=?1",
            [candidate.id.to_string()],
            |row| row.get(0),
        )?;
        let mut durable_tags = tx
            .prepare("SELECT tag FROM session_tags WHERE session_id=?1 ORDER BY tag")?
            .query_map([candidate.id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        durable_tags.sort();
        expected_tags.sort();
        let invocation_id_string = invocation_id.to_string();
        if candidate.id != current.candidate_session_id
            || candidate.session_kind != current.candidate_kind
            || !is_leaf_kind(candidate.session_kind)
            || candidate.parent_id != Some(current.epic_id)
            || candidate.continued_from != Some(current.predecessor_session_id)
            || candidate.provider != frozen.provider()?
            || candidate.project_id != frozen.project_id
            || candidate.agent_role != frozen.agent_role
            || candidate.epic_spawn_ordinal != frozen.epic_spawn_ordinal
            || candidate.working_dir.as_path() != std::path::Path::new(&frozen.working_dir)
            || candidate.query != current.request.query
            || candidate.model != current.request.model.clone().or(frozen.model)
            || candidate.effort != current.request.effort.clone().or(frozen.effort)
            || durable_tags != expected_tags
            || candidate.workflow_id != frozen.workflow_id
            || candidate.topology_node_id != current.request.topology_node
            || candidate.topology_iteration != current.request.iteration.unwrap_or(0)
            || candidate.is_eval != frozen.is_eval
            || candidate.status != SessionStatus::Starting
            || (frozen.rotation_disabled_at.is_some()
                && candidate.rotation_disabled_at != frozen.rotation_disabled_at)
            || candidate_invocation.as_deref() != Some(invocation_id_string.as_str())
        {
            return Err(DaemonError::PolicyDenied(format!(
                "agent_successor_provider_effect_candidate_mismatch:{}",
                current.candidate_session_id
            )));
        }
        if !agent_successor_live_custody_matches_on(&tx, candidate.id)? {
            return Err(DaemonError::PolicyDenied(
                "agent_successor_custody_mismatch".into(),
            ));
        }

        let invocation_matches: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM model_invocations
             WHERE id=?1 AND session_id=?2 AND purpose='agent.reserve_successor'
               AND admission_status='admitted' AND status='running')",
            params![invocation_id_string, candidate.id.to_string()],
            |row| row.get(0),
        )?;
        let epic = get_session_on(&tx, current.epic_id)?.ok_or_else(|| {
            DaemonError::PolicyDenied(format!(
                "agent_successor_provider_effect_epic_missing:{}",
                current.epic_id
            ))
        })?;
        let lead_generation: i64 = tx.query_row(
            "SELECT generation FROM epic_lead_generations WHERE epic_id=?1",
            [current.epic_id.to_string()],
            |row| row.get(0),
        )?;
        if !invocation_matches
            || epic.session_kind != SessionKind::Epic
            || epic.lead_session_id != Some(current.expected_lead_session_id)
            || current.expected_lead_session_id != current.predecessor_session_id
            || u64::try_from(lead_generation).ok() != Some(current.expected_lead_generation)
        {
            return Err(DaemonError::PolicyDenied(format!(
                "agent_successor_provider_effect_authority_mismatch:{}",
                current.reservation_id
            )));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn reserve_agent_successor(
        &self,
        caller_session_id: Uuid,
        request: &AgentReserveSuccessorRequestV1,
        ids: AgentSuccessorReservationIds,
    ) -> Result<ReserveAgentSuccessorOutcome> {
        request
            .validate()
            .map_err(|class| DaemonError::InvalidParam(class.into()))?;
        if !is_leaf_kind(request.kind)
            || !legal_children(Some(SessionKind::Epic)).contains(&request.kind)
        {
            return Err(DaemonError::InvalidParam(
                "agent_successor_kind_must_be_epic_leaf".into(),
            ));
        }
        if ids.reservation_id.is_nil()
            || ids.candidate_session_id.is_nil()
            || ids.transition_id.is_nil()
        {
            return Err(DaemonError::InvalidParam(
                "agent_successor_preallocated_ids_must_be_non_nil".into(),
            ));
        }
        let request_json = serde_json::to_string(request)?;
        let request_fingerprint = digest(&["agent.successor.request.v1", &request_json]);
        let caller_key_digest = digest(&[
            "agent.successor.idempotency.v1",
            &caller_session_id.to_string(),
            &request.idempotency_key,
        ]);
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) = tx
            .query_row(
                &format!("SELECT {RESERVATION_SELECT} FROM agent_successor_reservations WHERE predecessor_session_id=?1 AND caller_key_digest=?2"),
                params![caller_session_id.to_string(), caller_key_digest],
                map_reservation,
            )
            .optional()?
        {
            if existing.request_fingerprint != request_fingerprint || existing.request != *request {
                return Err(DaemonError::InvalidParam(format!(
                    "agent_successor_idempotency_conflict:{}",
                    existing.reservation_id
                )));
            }
            tx.commit()?;
            return Ok(ReserveAgentSuccessorOutcome::Replayed(existing));
        }

        // Idempotence above is keyed by (predecessor, caller_key_digest), so an
        // exact replay has already returned. A *different* key from the same
        // predecessor would otherwise reserve, launch and commit a second
        // candidate — a second receipted `continued_from` successor, which is
        // the branch `manager_lineage_tip` refuses. Observed once in the
        // operator database at Epic lead generations 2 and 7, three days apart,
        // after the predecessor legitimately reacquired the lead. The earliest
        // reservation is canonical, here and in the V111 convergence.
        reject_agent_successor_predecessor_continuation_on(&tx, caller_session_id)?;

        let mut caller = get_session_on(&tx, caller_session_id)?
            .ok_or(DaemonError::SessionNotFound(caller_session_id))?;
        {
            let mut statement =
                tx.prepare("SELECT tag FROM session_tags WHERE session_id=?1 ORDER BY tag")?;
            caller.tags = statement
                .query_map([caller_session_id.to_string()], |row| row.get(0))?
                .collect::<std::result::Result<Vec<String>, _>>()?;
            caller.tag = caller.tags.first().cloned().unwrap_or_default();
        }
        if !is_leaf_kind(caller.session_kind) {
            return Err(DaemonError::PolicyDenied(
                "agent_successor_predecessor_must_be_leaf".into(),
            ));
        }
        let epic = owning_epic(&tx, &caller)?;
        if epic.lead_session_id != Some(caller_session_id) {
            return Err(DaemonError::PolicyDenied(
                "agent_successor_caller_is_not_current_epic_lead".into(),
            ));
        }
        let generation: i64 = tx.query_row(
            "SELECT generation FROM epic_lead_generations WHERE epic_id=?1",
            [epic.id.to_string()],
            |row| row.get(0),
        )?;
        let generation = u64::try_from(generation).map_err(|_| {
            DaemonError::Store("agent successor Epic lead generation is invalid".into())
        })?;
        let inherited_launch_json = inherited_launch_json(&caller)?;
        let now = now_string();
        tx.execute(
            "INSERT INTO agent_successor_reservations
             (reservation_id,predecessor_session_id,epic_id,candidate_session_id,caller_key_digest,request_json,request_fingerprint,candidate_kind,inherited_launch_json,expected_lead_session_id,expected_lead_generation,state,state_version,reserved_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?2,?10,'reserved',1,?11,?11)",
            params![
                ids.reservation_id.to_string(), caller_session_id.to_string(), epic.id.to_string(),
                ids.candidate_session_id.to_string(), caller_key_digest, request_json,
                request_fingerprint, session_kind_to_str(request.kind), inherited_launch_json,
                i64::try_from(generation).map_err(|_| DaemonError::Store("agent successor generation overflow".into()))?, now,
            ],
        )?;
        let record = get_reservation_on(&tx, ids.reservation_id)?
            .ok_or_else(|| DaemonError::Store("reserved agent successor disappeared".into()))?;
        insert_transition(
            &tx,
            &record,
            ids.transition_id,
            None,
            AgentSuccessorStateV1::Reserved,
            1,
            None,
            &now,
        )?;
        tx.commit()?;
        Ok(ReserveAgentSuccessorOutcome::Reserved(record))
    }

    pub(crate) fn get_agent_successor(
        &self,
        reservation_id: Uuid,
    ) -> Result<Option<AgentSuccessorReservation>> {
        Ok(self.conn.query_row(
            &format!("SELECT {RESERVATION_SELECT} FROM agent_successor_reservations WHERE reservation_id=?1"),
            [reservation_id.to_string()], map_reservation,
        ).optional()?)
    }

    pub(crate) fn find_agent_successors_by_predecessor(
        &self,
        predecessor_session_id: Uuid,
    ) -> Result<Vec<AgentSuccessorReservation>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {RESERVATION_SELECT} FROM agent_successor_reservations WHERE predecessor_session_id=?1 ORDER BY reserved_at,reservation_id"
        ))?;
        Ok(statement
            .query_map([predecessor_session_id.to_string()], map_reservation)?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn list_reconcilable_agent_successors(
        &self,
        cursor: Option<AgentSuccessorCursor>,
        limit: usize,
    ) -> Result<AgentSuccessorPage> {
        if limit == 0 || limit > AGENT_SUCCESSOR_RECONCILE_PAGE_MAX {
            return Err(DaemonError::InvalidParam(format!(
                "agent_successor_reconcile_limit_must_be_1_to_{AGENT_SUCCESSOR_RECONCILE_PAGE_MAX}"
            )));
        }
        let cursor_at =
            cursor.map(|value| value.updated_at.to_rfc3339_opts(SecondsFormat::Nanos, true));
        let cursor_id = cursor.map(|value| value.reservation_id.to_string());
        let mut statement = self.conn.prepare(&format!(
            "SELECT {RESERVATION_SELECT} FROM agent_successor_reservations
             WHERE state IN ('reserved','launching','uncertain')
               AND (?1 IS NULL OR updated_at>?1 OR (updated_at=?1 AND reservation_id>?2))
             ORDER BY updated_at,reservation_id LIMIT ?3"
        ))?;
        let mut reservations = statement
            .query_map(
                params![
                    cursor_at,
                    cursor_id,
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                map_reservation,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let next_cursor = (reservations.len() == limit)
            .then(|| {
                reservations.last().map(|record| AgentSuccessorCursor {
                    updated_at: record.updated_at,
                    reservation_id: record.reservation_id,
                })
            })
            .flatten();
        reservations.shrink_to_fit();
        Ok(AgentSuccessorPage {
            reservations,
            next_cursor,
        })
    }

    pub(crate) fn claim_agent_successor_launch(
        &self,
        reservation_id: Uuid,
        expected_version: u64,
        ids: AgentSuccessorLaunchIds,
    ) -> Result<ClaimAgentSuccessorOutcome> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(mut record) = get_reservation_on(&tx, reservation_id)? else {
            return Err(DaemonError::InvalidParam(format!(
                "agent_successor_unknown:{reservation_id}"
            )));
        };
        if record.state != AgentSuccessorStateV1::Reserved
            || record.state_version != expected_version
        {
            tx.commit()?;
            return Ok(ClaimAgentSuccessorOutcome::Stale(record));
        }
        let status_raw: String = tx.query_row(
            "SELECT status FROM sessions WHERE id=?1",
            [record.predecessor_session_id.to_string()],
            |row| row.get(0),
        )?;
        let status = super::row_mappers::str_to_session_status(&status_raw)?;
        match status {
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval => {
                tx.commit()?;
                return Ok(ClaimAgentSuccessorOutcome::NotReady(record));
            }
            SessionStatus::Completed
            | SessionStatus::Failed
            | SessionStatus::Interrupted
            | SessionStatus::Archived
            | SessionStatus::Deleted => {}
            _ => {
                return Err(DaemonError::Store(format!(
                    "agent successor predecessor has unsupported status: {status:?}"
                )));
            }
        }
        let now = now_string();
        let next_version = expected_version
            .checked_add(1)
            .ok_or_else(|| DaemonError::Store("agent successor version overflow".into()))?;
        record.launch_attempt_id = Some(ids.launch_attempt_id);
        record.model_invocation_id = Some(ids.model_invocation_id);
        let changed = tx.execute(
            "UPDATE agent_successor_reservations SET state='launching',state_version=?1,launch_attempt_id=?2,model_invocation_id=?3,updated_at=?4
             WHERE reservation_id=?5 AND state='reserved' AND state_version=?6",
            params![i64::try_from(next_version).unwrap_or(i64::MAX), ids.launch_attempt_id.to_string(), ids.model_invocation_id.to_string(), now, reservation_id.to_string(), i64::try_from(expected_version).unwrap_or(i64::MAX)],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "agent successor claim CAS lost inside immediate transaction".into(),
            ));
        }
        insert_transition(
            &tx,
            &record,
            ids.transition_id,
            Some(record.state),
            AgentSuccessorStateV1::Launching,
            next_version,
            None,
            &now,
        )?;
        let claimed = get_reservation_on(&tx, reservation_id)?
            .ok_or_else(|| DaemonError::Store("claimed agent successor disappeared".into()))?;
        tx.commit()?;
        Ok(ClaimAgentSuccessorOutcome::Claimed(claimed))
    }

    pub(crate) fn settle_agent_successor_uncertain(
        &self,
        reservation_id: Uuid,
        expected_version: u64,
        launch_attempt_id: Uuid,
        transition_id: Uuid,
        reason: &str,
        safe_error_class: &str,
    ) -> Result<AgentSuccessorReservation> {
        self.settle_agent_successor(
            reservation_id,
            expected_version,
            launch_attempt_id,
            transition_id,
            AgentSuccessorStateV1::Uncertain,
            reason,
            safe_error_class,
            false,
        )
    }

    pub(crate) fn settle_agent_successor_failed(
        &self,
        reservation_id: Uuid,
        expected_version: u64,
        launch_attempt_id: Uuid,
        transition_id: Uuid,
        reason: &str,
        safe_error_class: &str,
    ) -> Result<AgentSuccessorReservation> {
        self.settle_agent_successor(
            reservation_id,
            expected_version,
            launch_attempt_id,
            transition_id,
            AgentSuccessorStateV1::Failed,
            reason,
            safe_error_class,
            false,
        )
    }

    /// Terminally settle a successor that definitively failed before provider
    /// establishment. The candidate row and reservation transition together,
    /// so restart cannot expose a nonterminal candidate for a failed baton.
    pub(crate) fn settle_agent_successor_establishment_failed(
        &self,
        reservation_id: Uuid,
        expected_version: u64,
        launch_attempt_id: Uuid,
        transition_id: Uuid,
        reason: &str,
        safe_error_class: &str,
    ) -> Result<AgentSuccessorReservation> {
        self.settle_agent_successor(
            reservation_id,
            expected_version,
            launch_attempt_id,
            transition_id,
            AgentSuccessorStateV1::Failed,
            reason,
            safe_error_class,
            true,
        )
    }

    fn settle_agent_successor(
        &self,
        reservation_id: Uuid,
        expected_version: u64,
        launch_attempt_id: Uuid,
        transition_id: Uuid,
        next: AgentSuccessorStateV1,
        reason: &str,
        safe_error_class: &str,
        terminalize_candidate: bool,
    ) -> Result<AgentSuccessorReservation> {
        if reason.len() > 256 || safe_error_class.len() > 128 {
            return Err(DaemonError::InvalidParam(
                "agent successor settlement reason/error class exceeds bound".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = get_reservation_on(&tx, reservation_id)?.ok_or_else(|| {
            DaemonError::InvalidParam(format!("agent_successor_unknown:{reservation_id}"))
        })?;
        if record.state_version != expected_version
            || record.launch_attempt_id != Some(launch_attempt_id)
            || !record.state.may_transition_to(next)
        {
            return Err(DaemonError::InvalidParam(format!(
                "agent_successor_settlement_stale:{reservation_id}"
            )));
        }
        let next_version = expected_version
            .checked_add(1)
            .ok_or_else(|| DaemonError::Store("agent successor version overflow".into()))?;
        let now = now_string();
        if terminalize_candidate {
            tx.execute(
                "UPDATE sessions SET lead_session_id=NULL,updated_at=?1 WHERE lead_session_id=?2",
                params![now, record.candidate_session_id.to_string()],
            )?;
            let candidate_changed = tx.execute(
                "UPDATE sessions
                 SET status='Failed',stop_reason=?1,parent_id=?2,continued_from=?3,updated_at=?4
                 WHERE id=?5 AND status IN ('Starting','Running','WaitingApproval')",
                params![
                    reason,
                    record.epic_id.to_string(),
                    record.predecessor_session_id.to_string(),
                    now,
                    record.candidate_session_id.to_string()
                ],
            )?;
            let candidate_is_terminal = candidate_changed == 0
                && tx
                    .query_row(
                        "SELECT status IN ('Completed','Failed','Interrupted','Archived','Deleted') FROM sessions WHERE id=?1",
                        [record.candidate_session_id.to_string()],
                        |row| row.get::<_, bool>(0),
                    )
                    .optional()?
                    .unwrap_or(false);
            if candidate_changed != 1 && !candidate_is_terminal {
                return Err(DaemonError::Store(format!(
                    "agent_successor_candidate_terminal_settlement_failed:{}",
                    record.candidate_session_id
                )));
            }
        }
        let changed = tx.execute(
            "UPDATE agent_successor_reservations SET state=?1,state_version=?2,terminal_reason=?3,safe_error_class=?4,updated_at=?5
             WHERE reservation_id=?6 AND state=?7 AND state_version=?8 AND launch_attempt_id=?9",
            params![next.as_str(), i64::try_from(next_version).unwrap_or(i64::MAX), reason, safe_error_class, now, reservation_id.to_string(), record.state.as_str(), i64::try_from(expected_version).unwrap_or(i64::MAX), launch_attempt_id.to_string()],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "agent successor settlement CAS lost inside immediate transaction".into(),
            ));
        }
        insert_transition(
            &tx,
            &record,
            transition_id,
            Some(record.state),
            next,
            next_version,
            Some(reason),
            &now,
        )?;
        let settled = get_reservation_on(&tx, reservation_id)?
            .ok_or_else(|| DaemonError::Store("settled agent successor disappeared".into()))?;
        tx.commit()?;
        Ok(settled)
    }

    pub(crate) fn commit_agent_successor_authority(
        &self,
        reservation_id: Uuid,
        expected_version: u64,
        launch_attempt_id: Uuid,
        transition_id: Uuid,
        establishment_evidence: &serde_json::Value,
    ) -> Result<AgentSuccessorReservation> {
        let evidence_json = serde_json::to_string(establishment_evidence)?;
        if !establishment_evidence.is_object()
            || establishment_evidence
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
        {
            return Err(DaemonError::InvalidParam(
                "agent_successor_establishment_evidence_required".into(),
            ));
        }
        let evidence_digest = digest(&["agent.successor.establishment.v1", &evidence_json]);
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = get_reservation_on(&tx, reservation_id)?.ok_or_else(|| {
            DaemonError::InvalidParam(format!("agent_successor_unknown:{reservation_id}"))
        })?;
        if record.state_version != expected_version
            || !matches!(
                record.state,
                AgentSuccessorStateV1::Launching | AgentSuccessorStateV1::Uncertain
            )
            || record.launch_attempt_id != Some(launch_attempt_id)
        {
            return Err(DaemonError::InvalidParam(format!(
                "agent_successor_commit_stale:{reservation_id}"
            )));
        }
        let model_invocation_id = record.model_invocation_id.ok_or_else(|| {
            DaemonError::Store("agent successor launching row has no invocation".into())
        })?;
        let next_version = expected_version
            .checked_add(1)
            .ok_or_else(|| DaemonError::Store("agent successor version overflow".into()))?;
        let now = now_string();
        let candidate = get_session_on(&tx, record.candidate_session_id)?
            .ok_or_else(|| DaemonError::InvalidParam("agent_successor_candidate_missing".into()))?;
        if candidate.session_kind != record.candidate_kind
            || !is_leaf_kind(candidate.session_kind)
            || candidate.parent_id != Some(record.epic_id)
            || candidate.continued_from != Some(record.predecessor_session_id)
            || !matches!(
                candidate.status,
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
            )
        {
            return Err(DaemonError::InvalidParam(
                "agent_successor_candidate_topology_mismatch".into(),
            ));
        }
        let invocation_matches: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM model_invocations WHERE id=?1 AND session_id=?2 AND purpose='agent.reserve_successor' AND admission_status='admitted')",
            params![model_invocation_id.to_string(), candidate.id.to_string()], |row| row.get(0),
        )?;
        let candidate_invocation: Option<String> = tx.query_row(
            "SELECT model_invocation_id FROM sessions WHERE id=?1",
            [candidate.id.to_string()],
            |row| row.get(0),
        )?;
        if !invocation_matches
            || candidate_invocation.as_deref() != Some(&model_invocation_id.to_string())
        {
            return Err(DaemonError::InvalidParam(
                "agent_successor_model_invocation_mismatch".into(),
            ));
        }
        if !agent_successor_live_custody_matches_on(&tx, candidate.id)? {
            return Err(DaemonError::InvalidParam(
                "agent_successor_custody_mismatch".into(),
            ));
        }
        commit_fault(AgentSuccessorCommitFault::AfterCandidateValidation)?;

        let changed = tx.execute(
            "UPDATE sessions SET lead_session_id=?1,updated_at=?2 WHERE id=?3 AND session_kind='Epic' AND lead_session_id=?4
             AND EXISTS(SELECT 1 FROM epic_lead_generations WHERE epic_id=?3 AND generation=?5)",
            params![record.candidate_session_id.to_string(), now, record.epic_id.to_string(), record.expected_lead_session_id.to_string(), i64::try_from(record.expected_lead_generation).unwrap_or(i64::MAX)],
        )?;
        if changed != 1 {
            drop(tx);
            let _ = self.settle_stale_authority(record.clone(), transition_id);
            return Err(DaemonError::PolicyDenied(format!(
                "agent_successor_authority_stale:{reservation_id}"
            )));
        }
        let next_generation: i64 = tx.query_row(
            "SELECT generation FROM epic_lead_generations WHERE epic_id=?1",
            [record.epic_id.to_string()],
            |row| row.get(0),
        )?;
        if u64::try_from(next_generation).ok() != record.expected_lead_generation.checked_add(1) {
            return Err(DaemonError::Store(
                "agent successor lead generation trigger did not advance exactly once".into(),
            ));
        }
        commit_fault(AgentSuccessorCommitFault::AfterLeadCas)?;
        let aggregate_changed = tx.execute(
            "UPDATE agent_successor_reservations SET state='committed',state_version=?1,establishment_evidence_json=?2,establishment_digest=?3,terminal_reason=NULL,safe_error_class=NULL,updated_at=?4
             WHERE reservation_id=?5 AND state=?6 AND state_version=?7 AND launch_attempt_id=?8 AND model_invocation_id=?9",
            params![i64::try_from(next_version).unwrap_or(i64::MAX), evidence_json, evidence_digest, now, reservation_id.to_string(), record.state.as_str(), i64::try_from(expected_version).unwrap_or(i64::MAX), launch_attempt_id.to_string(), model_invocation_id.to_string()],
        )?;
        if aggregate_changed != 1 {
            return Err(DaemonError::Store(
                "agent successor authority aggregate CAS lost".into(),
            ));
        }
        commit_fault(AgentSuccessorCommitFault::AfterAggregateCas)?;
        let mut transition_record = record.clone();
        transition_record.establishment_evidence_json = Some(evidence_json);
        transition_record.establishment_digest = Some(evidence_digest);
        insert_transition(
            &tx,
            &transition_record,
            transition_id,
            Some(record.state),
            AgentSuccessorStateV1::Committed,
            next_version,
            None,
            &now,
        )?;
        commit_fault(AgentSuccessorCommitFault::AfterAuditInsert)?;
        let committed = get_reservation_on(&tx, reservation_id)?
            .ok_or_else(|| DaemonError::Store("committed agent successor disappeared".into()))?;
        commit_fault(AgentSuccessorCommitFault::BeforeCommit)?;
        tx.commit()?;
        Ok(committed)
    }

    fn settle_stale_authority(
        &self,
        record: AgentSuccessorReservation,
        transition_id: Uuid,
    ) -> Result<()> {
        let Some(attempt) = record.launch_attempt_id else {
            return Ok(());
        };
        self.settle_agent_successor_failed(
            record.reservation_id,
            record.state_version,
            attempt,
            transition_id,
            "authority witness changed before baton transfer",
            "agent_successor_authority_stale",
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot};
    use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionProvider};

    struct World {
        store: Store,
        epic_id: Uuid,
        predecessor_id: Uuid,
        reservation_ids: AgentSuccessorReservationIds,
        request: AgentReserveSuccessorRequestV1,
    }

    fn world_at(path: Option<&std::path::Path>) -> World {
        let store = path
            .map(Store::open)
            .unwrap_or_else(Store::open_in_memory)
            .unwrap();
        let mut epic = crate::store::tests::make_test_session();
        epic.id = Uuid::new_v4();
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Completed;
        epic.project_id = None;
        epic.parent_id = None;
        epic.lead_session_id = None;
        store.insert_session(&epic).unwrap();

        let mut predecessor = crate::store::tests::make_test_session();
        predecessor.id = Uuid::new_v4();
        predecessor.session_kind = SessionKind::Task;
        predecessor.provider = SessionProvider::Codex;
        predecessor.status = SessionStatus::Running;
        predecessor.project_id = None;
        predecessor.parent_id = Some(epic.id);
        predecessor.lead_session_id = None;
        predecessor.agent_role = Some("Reviewer".into());
        predecessor.epic_spawn_ordinal = Some(7);
        store.insert_session(&predecessor).unwrap();
        store
            .set_lead_session(epic.id, Some(predecessor.id))
            .unwrap();

        World {
            store,
            epic_id: epic.id,
            predecessor_id: predecessor.id,
            reservation_ids: AgentSuccessorReservationIds {
                reservation_id: Uuid::new_v4(),
                candidate_session_id: Uuid::new_v4(),
                transition_id: Uuid::new_v4(),
            },
            request: AgentReserveSuccessorRequestV1 {
                kind: SessionKind::Task,
                model: Some("gpt-6-astra".into()),
                effort: Some("high".into()),
                query: "continue the master program".into(),
                topology_node: Some("implement".into()),
                iteration: Some(2),
                tags: Some(vec!["successor".into()]),
                idempotency_key: "master-baton-1".into(),
            },
        }
    }

    fn reserve(world: &World) -> AgentSuccessorReservation {
        match world
            .store
            .reserve_agent_successor(world.predecessor_id, &world.request, world.reservation_ids)
            .unwrap()
        {
            ReserveAgentSuccessorOutcome::Reserved(record) => record,
            ReserveAgentSuccessorOutcome::Replayed(_) => panic!("first reservation replayed"),
        }
    }

    fn claim(world: &World) -> (AgentSuccessorReservation, AgentSuccessorLaunchIds) {
        let reserved = reserve(world);
        world
            .store
            .update_session_status(world.predecessor_id, SessionStatus::Completed)
            .unwrap();
        let ids = AgentSuccessorLaunchIds {
            launch_attempt_id: Uuid::new_v4(),
            model_invocation_id: Uuid::new_v4(),
            transition_id: Uuid::new_v4(),
        };
        let ClaimAgentSuccessorOutcome::Claimed(record) = world
            .store
            .claim_agent_successor_launch(reserved.reservation_id, reserved.state_version, ids)
            .unwrap()
        else {
            panic!("settled predecessor did not claim");
        };
        (record, ids)
    }

    fn candidate_custody_binding(candidate: &mut Session) -> SessionCustodyBinding {
        let sandbox_root = std::path::PathBuf::from(format!("/tmp/{}", candidate.id));
        let sandbox_branch = format!("rsi/{}", candidate.id.simple());
        candidate.sandbox_kind = Some(SandboxKind::GitWorktree);
        candidate.sandbox_root = Some(sandbox_root.clone());
        candidate.sandbox_branch = Some(sandbox_branch.clone());
        candidate.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        SessionCustodyBinding::New(NewCustodyRoot {
            custody_id: Uuid::new_v4(),
            canonical_repo_dir: candidate.working_dir.to_string_lossy().to_string(),
            sandbox_root: sandbox_root.to_string_lossy().to_string(),
            sandbox_branch,
            repository_identity: format!("/tmp/test-git-common-{}", candidate.id),
            source_commit: "a".repeat(40),
            cause: CustodyCause::FreshLaunch,
        })
    }

    fn binding_custody_id(binding: &SessionCustodyBinding) -> Uuid {
        match binding {
            SessionCustodyBinding::New(root) => root.custody_id,
            _ => panic!("successor fixture requires a New binding"),
        }
    }

    fn assert_candidate_custody(world: &World, candidate_id: Uuid, custody_id: Uuid) {
        let custody = world
            .store
            .live_custody_for_session(candidate_id)
            .expect("candidate live custody");
        assert_eq!(custody.custody_id, custody_id);
        assert_eq!(custody.allocation_session_id, candidate_id);
        assert_eq!(custody.allocation_id, candidate_id);
        assert_eq!(custody.owner_session_id, candidate_id);
        assert_eq!(custody.generation, 1);

        let exact_witness: i64 = world
            .store
            .conn
            .query_row(
                "SELECT count(*)
                 FROM sessions s
                 JOIN sandbox_custody_roots r ON r.custody_id=s.sandbox_custody_id
                 JOIN sandbox_custody_events e ON e.custody_id=r.custody_id AND e.sequence=1
                 JOIN session_execution_projections p ON p.session_id=s.id
                 WHERE s.id=?1 AND r.custody_id=?2
                   AND s.working_dir=r.canonical_repo_dir
                   AND s.sandbox_root=r.sandbox_root
                   AND s.sandbox_branch=r.sandbox_branch
                   AND r.state='live' AND r.owner_session_id=s.id AND r.generation=1
                   AND r.validation_state='verified' AND r.validated_generation=1
                   AND e.event_kind='allocated' AND e.cause='fresh_launch'
                   AND e.from_generation IS NULL AND e.to_generation=1
                   AND e.from_owner_session_id IS NULL AND e.to_owner_session_id=s.id
                   AND p.execution_state='live_sandboxed' AND p.freshness='verified'
                   AND p.effective_cwd=r.sandbox_root AND p.custody_id=r.custody_id
                   AND p.custody_generation=1 AND p.error_code IS NULL",
                params![candidate_id.to_string(), custody_id.to_string()],
                |row| row.get(0),
            )
            .expect("load exact successor custody witness");
        assert_eq!(exact_witness, 1);
    }

    fn install_candidate(world: &mut World, record: &AgentSuccessorReservation) {
        let invocation_id = record.model_invocation_id.unwrap();
        let frozen = record.inherited_launch().unwrap();
        let mut candidate = crate::store::tests::make_test_session();
        candidate.id = record.candidate_session_id;
        candidate.session_kind = record.candidate_kind;
        candidate.status = SessionStatus::Running;
        candidate.project_id = None;
        candidate.parent_id = Some(record.epic_id);
        candidate.continued_from = Some(record.predecessor_session_id);
        candidate.provider = SessionProvider::Codex;
        candidate.agent_role = frozen.agent_role;
        candidate.epic_spawn_ordinal = frozen.epic_spawn_ordinal;
        let binding = candidate_custody_binding(&mut candidate);
        world
            .store
            .insert_session_with_custody(&candidate, binding)
            .unwrap();
        world
            .store
            .conn
            .execute(
                "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at)
                 VALUES(?1,'agent.reserve_successor','orchestration','foreground','paid','admitted','running','agent_reserve_successor',?2,?3)",
                params![
                    invocation_id.to_string(),
                    candidate.id.to_string(),
                    now_string(),
                ],
            )
            .unwrap();
        world
            .store
            .set_session_model_invocation(candidate.id, Some(invocation_id))
            .unwrap();
    }

    fn install_boundary_invocation(
        world: &World,
        record: &AgentSuccessorReservation,
    ) -> (Session, SessionCustodyBinding) {
        let invocation_id = record.model_invocation_id.unwrap();
        world
            .store
            .conn
            .execute(
                "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at)
                 VALUES(?1,'agent.reserve_successor','orchestration','foreground','paid','admitted','running','agent_reserve_successor',?2,?3)",
                params![
                    invocation_id.to_string(),
                    record.candidate_session_id.to_string(),
                    now_string(),
                ],
            )
            .unwrap();
        let frozen = record.inherited_launch().unwrap();
        let mut candidate = crate::store::tests::make_test_session();
        candidate.id = record.candidate_session_id;
        candidate.session_kind = record.candidate_kind;
        candidate.status = SessionStatus::Starting;
        candidate.project_id = frozen.project_id;
        candidate.parent_id = Some(record.epic_id);
        candidate.continued_from = Some(record.predecessor_session_id);
        candidate.provider = frozen.provider().unwrap();
        candidate.working_dir = std::path::PathBuf::from(frozen.working_dir);
        candidate.query = record.request.query.clone();
        candidate.model = record.request.model.clone().or(frozen.model);
        candidate.effort = record.request.effort.clone().or(frozen.effort);
        candidate.agent_role = frozen.agent_role;
        candidate.epic_spawn_ordinal = frozen.epic_spawn_ordinal;
        candidate.tags = record.request.tags.clone().unwrap_or(frozen.tags);
        candidate.tag = candidate.tags.first().cloned().unwrap_or_default();
        candidate.topology_node_id = record.request.topology_node.clone();
        candidate.topology_iteration = record.request.iteration.unwrap_or(0);
        let binding = candidate_custody_binding(&mut candidate);
        (candidate, binding)
    }

    /// A predecessor publishes exactly one continuation. A different
    /// idempotency key is a second baton, not a retry, and is refused; the
    /// exact key still replays its own receipt, and a `failed` reservation
    /// releases the predecessor so ordinary rotation stays available.
    #[test]
    fn successor_reserve_admits_one_continuation_per_predecessor() {
        let world = world_at(None);
        let first = reserve(&world);

        let mut second_request = world.request.clone();
        second_request.idempotency_key = "master-baton-2".into();
        second_request.query = "continue the master program again".into();
        let refusal = world
            .store
            .reserve_agent_successor(
                world.predecessor_id,
                &second_request,
                AgentSuccessorReservationIds {
                    reservation_id: Uuid::new_v4(),
                    candidate_session_id: Uuid::new_v4(),
                    transition_id: Uuid::new_v4(),
                },
            )
            .expect_err("a second baton under a new key must be refused");
        assert!(
            format!("{refusal}").contains(&format!(
                "agent_successor_predecessor_already_continued:{}:{}",
                world.predecessor_id, first.reservation_id
            )),
            "refusal must name the predecessor and its one reservation: {refusal}"
        );

        let replay = world
            .store
            .reserve_agent_successor(
                world.predecessor_id,
                &world.request,
                AgentSuccessorReservationIds {
                    reservation_id: Uuid::new_v4(),
                    candidate_session_id: Uuid::new_v4(),
                    transition_id: Uuid::new_v4(),
                },
            )
            .expect("the exact key still replays");
        let ReserveAgentSuccessorOutcome::Replayed(replayed) = replay else {
            panic!("exact retry allocated a second reservation");
        };
        assert_eq!(replayed.reservation_id, first.reservation_id);

        assert_eq!(
            world
                .store
                .find_agent_successors_by_predecessor(world.predecessor_id)
                .unwrap()
                .into_iter()
                .map(|record| record.reservation_id)
                .collect::<Vec<_>>(),
            vec![first.reservation_id],
            "the predecessor keeps exactly one reservation"
        );

        // Settling that one reservation releases the predecessor: a failed
        // baton transferred no authority and must not disable rotation, or a
        // fresh retry, for the rest of the session's life.
        world
            .store
            .update_session_status(world.predecessor_id, SessionStatus::Completed)
            .unwrap();
        let launch_ids = AgentSuccessorLaunchIds {
            launch_attempt_id: Uuid::new_v4(),
            model_invocation_id: Uuid::new_v4(),
            transition_id: Uuid::new_v4(),
        };
        let ClaimAgentSuccessorOutcome::Claimed(launching) = world
            .store
            .claim_agent_successor_launch(first.reservation_id, first.state_version, launch_ids)
            .unwrap()
        else {
            panic!("claim did not launch");
        };
        world
            .store
            .settle_agent_successor_failed(
                launching.reservation_id,
                launching.state_version,
                launch_ids.launch_attempt_id,
                Uuid::new_v4(),
                "provider failed before establishment",
                "successor_failed",
            )
            .unwrap();
        let retry_ids = AgentSuccessorReservationIds {
            reservation_id: Uuid::new_v4(),
            candidate_session_id: Uuid::new_v4(),
            transition_id: Uuid::new_v4(),
        };
        let ReserveAgentSuccessorOutcome::Reserved(retry) = world
            .store
            .reserve_agent_successor(world.predecessor_id, &second_request, retry_ids)
            .expect("a settled reservation releases the predecessor")
        else {
            panic!("fresh post-failure reservation replayed");
        };
        assert_eq!(retry.reservation_id, retry_ids.reservation_id);
    }

    #[test]
    fn successor_reserve_replay_conflict_and_no_auth_writes() {
        let world = world_at(None);
        let record = reserve(&world);
        assert_eq!(record.epic_id, world.epic_id);
        assert_eq!(record.expected_lead_generation, 2);
        assert_eq!(record.state_version, 1);
        assert_eq!(record.receipt(false).deduplicated, false);
        let inherited = record.inherited_launch().unwrap();
        assert_eq!(inherited.agent_role.as_deref(), Some("Reviewer"));
        assert_eq!(inherited.epic_spawn_ordinal, Some(7));

        let replay = world
            .store
            .reserve_agent_successor(
                world.predecessor_id,
                &world.request,
                AgentSuccessorReservationIds {
                    reservation_id: Uuid::new_v4(),
                    candidate_session_id: Uuid::new_v4(),
                    transition_id: Uuid::new_v4(),
                },
            )
            .unwrap();
        let ReserveAgentSuccessorOutcome::Replayed(replayed) = replay else {
            panic!("exact retry allocated a second reservation");
        };
        assert_eq!(replayed.reservation_id, record.reservation_id);
        assert!(replayed.receipt(true).deduplicated);

        let mut changed = world.request.clone();
        changed.query.push_str(" changed");
        let error = world
            .store
            .reserve_agent_successor(
                world.predecessor_id,
                &changed,
                AgentSuccessorReservationIds {
                    reservation_id: Uuid::new_v4(),
                    candidate_session_id: Uuid::new_v4(),
                    transition_id: Uuid::new_v4(),
                },
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("agent_successor_idempotency_conflict")
        );

        let mut non_lead = crate::store::tests::make_test_session();
        non_lead.id = Uuid::new_v4();
        non_lead.session_kind = SessionKind::Task;
        non_lead.project_id = None;
        non_lead.parent_id = Some(world.epic_id);
        world.store.insert_session(&non_lead).unwrap();
        assert!(
            world
                .store
                .reserve_agent_successor(
                    non_lead.id,
                    &world.request,
                    AgentSuccessorReservationIds {
                        reservation_id: Uuid::new_v4(),
                        candidate_session_id: Uuid::new_v4(),
                        transition_id: Uuid::new_v4(),
                    },
                )
                .is_err()
        );
        let count: i64 = world
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM agent_successor_reservations",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "authorization failure wrote a reservation");
    }

    #[test]
    fn successor_v92_ledger_rejects_noncanonical_values_and_invalid_state_shapes() {
        let world = world_at(None);
        let digest = format!("sha256:{}", "a".repeat(64));
        let timestamp = "2026-08-23T12:34:56.123456789Z";
        let insert_reservation = |reservation_id: &str,
                                  caller_digest: &str,
                                  state: &str,
                                  launch_attempt_id: Option<&str>,
                                  model_invocation_id: Option<&str>,
                                  evidence_json: Option<&str>,
                                  evidence_digest: Option<&str>,
                                  terminal_reason: Option<&str>,
                                  safe_error_class: Option<&str>,
                                  reserved_at: &str| {
            world.store.conn.execute(
                "INSERT INTO agent_successor_reservations
                 (reservation_id,predecessor_session_id,epic_id,candidate_session_id,
                  caller_key_digest,request_json,request_fingerprint,candidate_kind,
                  inherited_launch_json,expected_lead_session_id,expected_lead_generation,
                  state,state_version,launch_attempt_id,model_invocation_id,
                  establishment_evidence_json,establishment_digest,terminal_reason,
                  safe_error_class,reserved_at,updated_at)
                 VALUES(?1,?2,?3,?4,?5,'{}',?6,'Task','{}',?2,2,?7,1,?8,?9,
                        ?10,?11,?12,?13,?14,?14)",
                params![
                    reservation_id,
                    world.predecessor_id.to_string(),
                    world.epic_id.to_string(),
                    Uuid::new_v4().to_string(),
                    caller_digest,
                    digest,
                    state,
                    launch_attempt_id,
                    model_invocation_id,
                    evidence_json,
                    evidence_digest,
                    terminal_reason,
                    safe_error_class,
                    reserved_at,
                ],
            )
        };

        for result in [
            insert_reservation(
                "00000000-0000-0000-0000-00000000000g",
                &digest,
                "reserved",
                None,
                None,
                None,
                None,
                None,
                None,
                timestamp,
            ),
            insert_reservation(
                &Uuid::new_v4().to_string(),
                &format!("sha256:{}", "g".repeat(64)),
                "reserved",
                None,
                None,
                None,
                None,
                None,
                None,
                timestamp,
            ),
            insert_reservation(
                &Uuid::new_v4().to_string(),
                &digest,
                "reserved",
                None,
                None,
                None,
                None,
                None,
                None,
                "2026-99-23T12:34:56.123456789Z",
            ),
            insert_reservation(
                &Uuid::new_v4().to_string(),
                &digest,
                "reserved",
                Some(&Uuid::new_v4().to_string()),
                Some(&Uuid::new_v4().to_string()),
                None,
                None,
                None,
                None,
                timestamp,
            ),
            insert_reservation(
                &Uuid::new_v4().to_string(),
                &digest,
                "committed",
                Some(&Uuid::new_v4().to_string()),
                Some(&Uuid::new_v4().to_string()),
                None,
                None,
                None,
                None,
                timestamp,
            ),
        ] {
            assert!(result.is_err(), "malformed V92 reservation was admitted");
        }

        let reserved = reserve(&world);
        let malformed_transition = world.store.conn.execute(
            "INSERT INTO agent_successor_transitions
             (transition_id,reservation_id,state_version,from_state,to_state,authority_digest,
              launch_attempt_id,model_invocation_id,reason,created_at)
             VALUES(?1,?2,2,'reserved','launching',?3,?4,?5,NULL,?6)",
            params![
                Uuid::new_v4().to_string(),
                reserved.reservation_id.to_string(),
                format!("sha256:{}", "g".repeat(64)),
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                timestamp,
            ],
        );
        assert!(
            malformed_transition.is_err(),
            "malformed V92 transition digest was admitted"
        );
    }

    #[test]
    fn successor_v92_ledger_rejects_disconnected_aggregate_and_transition_rows() {
        let timestamp = "2099-08-23T12:34:56.123456789Z";
        let authority_digest = format!("sha256:{}", "a".repeat(64));

        let world = world_at(None);
        let reserved = reserve(&world);
        let tx =
            Transaction::new_unchecked(&world.store.conn, TransactionBehavior::Immediate).unwrap();
        tx.execute(
            "INSERT INTO agent_successor_reservations
             SELECT ?1,predecessor_session_id,epic_id,?2,?3,request_json,
                    request_fingerprint,candidate_kind,inherited_launch_json,
                    expected_lead_session_id,expected_lead_generation,state,state_version,
                    launch_attempt_id,model_invocation_id,establishment_evidence_json,
                    establishment_digest,published_at,terminal_reason,safe_error_class,
                    reserved_at,updated_at
             FROM agent_successor_reservations WHERE reservation_id=?4",
            params![
                Uuid::new_v4().to_string(),
                Uuid::new_v4().to_string(),
                format!("sha256:{}", "b".repeat(64)),
                reserved.reservation_id.to_string(),
            ],
        )
        .unwrap();
        assert!(
            tx.commit().is_err(),
            "reservation without its initial transition committed"
        );

        let world = world_at(None);
        let reserved = reserve(&world);
        let tx =
            Transaction::new_unchecked(&world.store.conn, TransactionBehavior::Immediate).unwrap();
        tx.execute(
            "UPDATE agent_successor_reservations
             SET state='failed',state_version=2,terminal_reason='hostile',
                 safe_error_class='hostile',updated_at=?1
             WHERE reservation_id=?2",
            params![timestamp, reserved.reservation_id.to_string()],
        )
        .unwrap();
        assert!(
            tx.commit().is_err(),
            "aggregate without its current transition committed"
        );

        let reject_launch_transition =
            |state_version: i64, mismatch_attempt: bool, mismatch_invocation: bool| {
                let world = world_at(None);
                let reserved = reserve(&world);
                let aggregate_attempt = Uuid::new_v4();
                let aggregate_invocation = Uuid::new_v4();
                let tx =
                    Transaction::new_unchecked(&world.store.conn, TransactionBehavior::Immediate)
                        .unwrap();
                tx.execute(
                    "UPDATE agent_successor_reservations
                     SET state='launching',state_version=2,launch_attempt_id=?1,
                         model_invocation_id=?2,updated_at=?3
                     WHERE reservation_id=?4",
                    params![
                        aggregate_attempt.to_string(),
                        aggregate_invocation.to_string(),
                        timestamp,
                        reserved.reservation_id.to_string(),
                    ],
                )
                .unwrap();
                let transition_attempt = mismatch_attempt
                    .then(Uuid::new_v4)
                    .unwrap_or(aggregate_attempt);
                let transition_invocation = mismatch_invocation
                    .then(Uuid::new_v4)
                    .unwrap_or(aggregate_invocation);
                let result = tx.execute(
                    "INSERT INTO agent_successor_transitions
                     (transition_id,reservation_id,state_version,from_state,to_state,
                      authority_digest,launch_attempt_id,model_invocation_id,reason,created_at)
                     VALUES(?1,?2,?3,'reserved','launching',?4,?5,?6,NULL,?7)",
                    params![
                        Uuid::new_v4().to_string(),
                        reserved.reservation_id.to_string(),
                        state_version,
                        authority_digest,
                        transition_attempt.to_string(),
                        transition_invocation.to_string(),
                        timestamp,
                    ],
                );
                result
            };

        let result = reject_launch_transition(3, false, false);
        assert!(
            result.is_err(),
            "disconnected transition version was admitted"
        );
        let result = reject_launch_transition(2, true, false);
        assert!(result.is_err(), "mismatched launch attempt was admitted");
        let result = reject_launch_transition(2, false, true);
        assert!(result.is_err(), "mismatched model invocation was admitted");

        let world = world_at(None);
        let reserved = reserve(&world);
        let tx =
            Transaction::new_unchecked(&world.store.conn, TransactionBehavior::Immediate).unwrap();
        tx.execute(
            "UPDATE agent_successor_reservations
             SET state='failed',state_version=2,terminal_reason='hostile',
                 safe_error_class='hostile',updated_at=?1
             WHERE reservation_id=?2",
            params![timestamp, reserved.reservation_id.to_string()],
        )
        .unwrap();
        let result = tx.execute(
            "INSERT INTO agent_successor_transitions
             (transition_id,reservation_id,state_version,from_state,to_state,
              authority_digest,launch_attempt_id,model_invocation_id,reason,created_at)
             VALUES(?1,?2,2,'launching','failed',?3,NULL,NULL,'hostile',?4)",
            params![
                Uuid::new_v4().to_string(),
                reserved.reservation_id.to_string(),
                authority_digest,
                timestamp,
            ],
        );
        assert!(result.is_err(), "disconnected from-state was admitted");

        let world = world_at(None);
        let reserved = reserve(&world);
        let attempt = Uuid::new_v4();
        let invocation = Uuid::new_v4();
        let tx =
            Transaction::new_unchecked(&world.store.conn, TransactionBehavior::Immediate).unwrap();
        tx.execute(
            "UPDATE agent_successor_reservations
             SET state='launching',state_version=2,launch_attempt_id=?1,
                 model_invocation_id=?2,updated_at=?3
             WHERE reservation_id=?4",
            params![
                attempt.to_string(),
                invocation.to_string(),
                timestamp,
                reserved.reservation_id.to_string(),
            ],
        )
        .unwrap();
        let result = tx.execute(
            "INSERT INTO agent_successor_transitions
             (transition_id,reservation_id,state_version,from_state,to_state,
              authority_digest,launch_attempt_id,model_invocation_id,reason,created_at)
             VALUES(?1,?2,2,'reserved','failed',?3,?4,?5,'hostile',?6)",
            params![
                Uuid::new_v4().to_string(),
                reserved.reservation_id.to_string(),
                authority_digest,
                attempt.to_string(),
                invocation.to_string(),
                timestamp,
            ],
        );
        assert!(result.is_err(), "disconnected to-state was admitted");

        let world = world_at(None);
        let (launching, _) = claim(&world);
        assert!(
            world
                .store
                .conn
                .execute(
                    "UPDATE agent_successor_reservations SET launch_attempt_id=?1
                     WHERE reservation_id=?2",
                    params![
                        Uuid::new_v4().to_string(),
                        launching.reservation_id.to_string()
                    ],
                )
                .is_err(),
            "current aggregate was detached from its transition"
        );
    }

    #[test]
    fn successor_claim_waits_for_settlement_and_is_exactly_once() {
        let world = world_at(None);
        let reserved = reserve(&world);
        let ids = AgentSuccessorLaunchIds {
            launch_attempt_id: Uuid::new_v4(),
            model_invocation_id: Uuid::new_v4(),
            transition_id: Uuid::new_v4(),
        };
        assert!(matches!(
            world
                .store
                .claim_agent_successor_launch(reserved.reservation_id, 1, ids)
                .unwrap(),
            ClaimAgentSuccessorOutcome::NotReady(_)
        ));
        assert_eq!(
            world
                .store
                .get_agent_successor(reserved.reservation_id)
                .unwrap()
                .unwrap()
                .state_version,
            1
        );

        for status in [
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Interrupted,
            SessionStatus::Archived,
            SessionStatus::Deleted,
        ] {
            let world = world_at(None);
            let reserved = reserve(&world);
            world
                .store
                .update_session_status(world.predecessor_id, status)
                .unwrap();
            let ids = AgentSuccessorLaunchIds {
                launch_attempt_id: Uuid::new_v4(),
                model_invocation_id: Uuid::new_v4(),
                transition_id: Uuid::new_v4(),
            };
            assert!(matches!(
                world
                    .store
                    .claim_agent_successor_launch(reserved.reservation_id, 1, ids)
                    .unwrap(),
                ClaimAgentSuccessorOutcome::Claimed(_)
            ));
            assert!(matches!(
                world
                    .store
                    .claim_agent_successor_launch(reserved.reservation_id, 1, ids)
                    .unwrap(),
                ClaimAgentSuccessorOutcome::Stale(_)
            ));
        }
    }

    #[test]
    fn successor_reservation_preserves_rotation_override_before_provider_effect() {
        let world = world_at(None);
        let disabled_at = Utc::now();
        world
            .store
            .set_session_rotation_disabled_at(world.predecessor_id, Some(disabled_at))
            .unwrap();
        let (launching, ids) = claim(&world);
        assert_eq!(
            launching.inherited_launch().unwrap().rotation_disabled_at,
            Some(disabled_at),
            "reservation must freeze the predecessor's explicit override"
        );

        let (mut candidate, binding) = install_boundary_invocation(&world, &launching);
        assert!(
            world
                .store
                .insert_agent_successor_session_with_custody(
                    &launching,
                    &candidate,
                    ids.model_invocation_id,
                    binding.clone(),
                )
                .unwrap_err()
                .to_string()
                .contains("agent_successor_pre_effect_binding_mismatch")
        );
        candidate.rotation_disabled_at = Some(disabled_at);
        world
            .store
            .insert_agent_successor_session_with_custody(
                &launching,
                &candidate,
                ids.model_invocation_id,
                binding,
            )
            .unwrap();
        assert_eq!(
            world
                .store
                .get_session(candidate.id)
                .unwrap()
                .unwrap()
                .rotation_disabled_at,
            Some(disabled_at)
        );
        world
            .store
            .validate_agent_successor_provider_effect_fence(&launching, ids.model_invocation_id)
            .unwrap();
    }

    #[test]
    fn successor_pre_effect_boundary_requires_exact_reservation_and_invocation() {
        let world = world_at(None);
        let (launching, ids) = claim(&world);
        let (candidate, binding) = install_boundary_invocation(&world, &launching);
        let custody_id = binding_custody_id(&binding);

        for non_new in [
            SessionCustodyBinding::Ordinary,
            SessionCustodyBinding::Reuse {
                custody_id: Uuid::new_v4(),
                generation: 1,
                cause: CustodyCause::FreshLaunch,
            },
            SessionCustodyBinding::Transfer {
                custody_id: Uuid::new_v4(),
                from_session_id: launching.predecessor_session_id,
                generation: 1,
                cause: CustodyCause::FreshLaunch,
                origin_session_id: None,
                scheduled_job_id: None,
            },
        ] {
            assert!(
                world
                    .store
                    .insert_agent_successor_session_with_custody(
                        &launching,
                        &candidate,
                        ids.model_invocation_id,
                        non_new,
                    )
                    .unwrap_err()
                    .to_string()
                    .contains("requires a new custody root")
            );
        }

        assert!(
            world
                .store
                .insert_agent_successor_session_with_custody(
                    &launching,
                    &candidate,
                    Uuid::new_v4(),
                    binding.clone(),
                )
                .unwrap_err()
                .to_string()
                .contains("agent_successor_pre_effect_binding_mismatch")
        );
        assert!(world.store.get_session(candidate.id).unwrap().is_none());

        let mut wrong_topology = candidate.clone();
        wrong_topology.parent_id = None;
        assert!(
            world
                .store
                .insert_agent_successor_session_with_custody(
                    &launching,
                    &wrong_topology,
                    ids.model_invocation_id,
                    binding.clone(),
                )
                .unwrap_err()
                .to_string()
                .contains("agent_successor_pre_effect_binding_mismatch")
        );
        assert!(world.store.get_session(candidate.id).unwrap().is_none());

        world
            .store
            .insert_agent_successor_session_with_custody(
                &launching,
                &candidate,
                ids.model_invocation_id,
                binding,
            )
            .unwrap();
        world
            .store
            .validate_agent_successor_provider_effect_fence(&launching, ids.model_invocation_id)
            .unwrap();
        assert_eq!(
            world
                .store
                .session_model_invocation_id(candidate.id)
                .unwrap(),
            Some(ids.model_invocation_id)
        );
        assert_candidate_custody(&world, candidate.id, custody_id);
        let durable_tags: Vec<String> = world
            .store
            .conn
            .prepare("SELECT tag FROM session_tags WHERE session_id=?1 ORDER BY tag")
            .unwrap()
            .query_map([candidate.id.to_string()], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(durable_tags, candidate.tags);
        assert!(
            world
                .store
                .update_session_parent(candidate.id, None)
                .unwrap_err()
                .to_string()
                .contains("agent_successor_candidate_topology_locked")
        );
        assert!(
            world
                .store
                .update_session_parent(world.predecessor_id, None)
                .unwrap_err()
                .to_string()
                .contains("agent_successor_candidate_topology_locked")
        );
        assert!(
            world
                .store
                .set_lead_session(world.epic_id, Some(candidate.id))
                .unwrap_err()
                .to_string()
                .contains("agent_successor_epic_lead_locked")
        );
        assert!(
            world
                .store
                .clear_lead_session_if_matches(world.predecessor_id)
                .unwrap_err()
                .to_string()
                .contains("agent_successor_epic_lead_locked")
        );
    }

    #[test]
    fn successor_pre_effect_custody_failure_rolls_back_candidate_aggregate() {
        let world = world_at(None);
        let (launching, ids) = claim(&world);
        let (candidate, mut binding) = install_boundary_invocation(&world, &launching);
        let custody_id = binding_custody_id(&binding);
        let SessionCustodyBinding::New(root) = &mut binding else {
            unreachable!("fixture always returns a New binding")
        };
        root.sandbox_branch.push_str("-mismatch");

        let error = world
            .store
            .insert_agent_successor_session_with_custody(
                &launching,
                &candidate,
                ids.model_invocation_id,
                binding,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("sandbox custody binding session tuple does not match root")
        );

        let partial_rows: (bool, bool, bool, bool, bool) = world
            .store
            .conn
            .query_row(
                "SELECT
                   EXISTS(SELECT 1 FROM sessions WHERE id=?1),
                   EXISTS(SELECT 1 FROM session_tags WHERE session_id=?1),
                   EXISTS(SELECT 1 FROM sandbox_custody_roots WHERE custody_id=?2),
                   EXISTS(SELECT 1 FROM sandbox_custody_events WHERE custody_id=?2),
                   EXISTS(SELECT 1 FROM session_execution_projections WHERE session_id=?1)",
                params![candidate.id.to_string(), custody_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(partial_rows, (false, false, false, false, false));
        let invocation_exists: bool = world
            .store
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM model_invocations WHERE id=?1)",
                [ids.model_invocation_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(invocation_exists, "the outer saga owns the invocation");
        assert_eq!(
            world
                .store
                .get_agent_successor(launching.reservation_id)
                .unwrap()
                .unwrap()
                .state,
            AgentSuccessorStateV1::Launching
        );
    }

    #[test]
    fn successor_provider_and_commit_fences_refuse_missing_custody() {
        let world = world_at(None);
        let (launching, ids) = claim(&world);
        let (candidate, _binding) = install_boundary_invocation(&world, &launching);
        world.store.insert_session(&candidate).unwrap();
        world
            .store
            .set_session_model_invocation(candidate.id, Some(ids.model_invocation_id))
            .unwrap();
        for tag in &candidate.tags {
            world
                .store
                .conn
                .execute(
                    "INSERT INTO session_tags(session_id,tag) VALUES(?1,?2)",
                    params![candidate.id.to_string(), tag],
                )
                .unwrap();
        }

        let effect_error = world
            .store
            .validate_agent_successor_provider_effect_fence(&launching, ids.model_invocation_id)
            .unwrap_err();
        assert!(
            effect_error
                .to_string()
                .contains("agent_successor_custody_mismatch")
        );
        let commit_error = world
            .store
            .commit_agent_successor_authority(
                launching.reservation_id,
                launching.state_version,
                ids.launch_attempt_id,
                Uuid::new_v4(),
                &serde_json::json!({"provider":"Codex","live":true}),
            )
            .unwrap_err();
        assert!(
            commit_error
                .to_string()
                .contains("agent_successor_custody_mismatch")
        );
        assert_eq!(
            world
                .store
                .get_session(world.epic_id)
                .unwrap()
                .unwrap()
                .lead_session_id,
            Some(world.predecessor_id)
        );
        assert_eq!(
            world
                .store
                .get_agent_successor(launching.reservation_id)
                .unwrap()
                .unwrap()
                .state,
            AgentSuccessorStateV1::Launching
        );
    }

    #[test]
    fn successor_settlement_and_reconcilable_reads_are_fenced() {
        let world = world_at(None);
        let (launching, ids) = claim(&world);
        let uncertain = world
            .store
            .settle_agent_successor_uncertain(
                launching.reservation_id,
                launching.state_version,
                ids.launch_attempt_id,
                Uuid::new_v4(),
                "provider effect may have crossed the boundary",
                "provider_effect_uncertain",
            )
            .unwrap();
        assert_eq!(uncertain.state, AgentSuccessorStateV1::Uncertain);
        assert!(
            world
                .store
                .settle_agent_successor_failed(
                    uncertain.reservation_id,
                    uncertain.state_version,
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    "wrong attempt",
                    "wrong_attempt",
                )
                .is_err()
        );
        let failed = world
            .store
            .settle_agent_successor_failed(
                uncertain.reservation_id,
                uncertain.state_version,
                ids.launch_attempt_id,
                Uuid::new_v4(),
                "reconciliation proved absence",
                "provider_absent",
            )
            .unwrap();
        assert_eq!(failed.state, AgentSuccessorStateV1::Failed);
        assert!(
            world
                .store
                .list_reconcilable_agent_successors(None, 1)
                .unwrap()
                .reservations
                .is_empty()
        );
        assert_eq!(
            world
                .store
                .find_agent_successors_by_predecessor(world.predecessor_id)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn successor_authority_commit_is_atomic_and_generation_fenced() {
        let mut world = world_at(None);
        let (launching, ids) = claim(&world);
        install_candidate(&mut world, &launching);
        let committed = world
            .store
            .commit_agent_successor_authority(
                launching.reservation_id,
                launching.state_version,
                ids.launch_attempt_id,
                Uuid::new_v4(),
                &serde_json::json!({"provider":"Codex","live":true}),
            )
            .unwrap();
        assert_eq!(committed.state, AgentSuccessorStateV1::Committed);
        assert_eq!(
            world
                .store
                .get_session(world.epic_id)
                .unwrap()
                .unwrap()
                .lead_session_id,
            Some(committed.candidate_session_id)
        );
        let generation: i64 = world
            .store
            .conn
            .query_row(
                "SELECT generation FROM epic_lead_generations WHERE epic_id=?1",
                [world.epic_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(generation, 3);
        let transitions: i64 = world
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM agent_successor_transitions WHERE reservation_id=?1",
                [committed.reservation_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(transitions, 3);
    }

    #[test]
    fn successor_authority_commit_rejects_aba_and_settles_stale() {
        let mut world = world_at(None);
        let (launching, ids) = claim(&world);
        install_candidate(&mut world, &launching);
        let replacement = Uuid::new_v4();
        world
            .store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?1,updated_at=?2 WHERE id=?3",
                params![
                    replacement.to_string(),
                    now_string(),
                    world.epic_id.to_string()
                ],
            )
            .unwrap();
        world
            .store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?1,updated_at=?2 WHERE id=?3",
                params![
                    world.predecessor_id.to_string(),
                    now_string(),
                    world.epic_id.to_string()
                ],
            )
            .unwrap();
        let error = world
            .store
            .commit_agent_successor_authority(
                launching.reservation_id,
                launching.state_version,
                ids.launch_attempt_id,
                Uuid::new_v4(),
                &serde_json::json!({"provider":"Codex","live":true}),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("agent_successor_authority_stale")
        );
        assert_eq!(
            world
                .store
                .get_session(world.epic_id)
                .unwrap()
                .unwrap()
                .lead_session_id,
            Some(world.predecessor_id)
        );
        assert_eq!(
            world
                .store
                .get_agent_successor(launching.reservation_id)
                .unwrap()
                .unwrap()
                .state,
            AgentSuccessorStateV1::Failed
        );
    }

    #[test]
    fn successor_authority_failpoints_roll_back_every_atomic_member() {
        for fault in [
            AgentSuccessorCommitFault::AfterAuditInsert,
            AgentSuccessorCommitFault::AfterCandidateValidation,
            AgentSuccessorCommitFault::AfterLeadCas,
            AgentSuccessorCommitFault::AfterAggregateCas,
            AgentSuccessorCommitFault::BeforeCommit,
        ] {
            let mut world = world_at(None);
            let (launching, ids) = claim(&world);
            install_candidate(&mut world, &launching);
            successor_test_fail_next_commit(fault);
            assert!(
                world
                    .store
                    .commit_agent_successor_authority(
                        launching.reservation_id,
                        launching.state_version,
                        ids.launch_attempt_id,
                        Uuid::new_v4(),
                        &serde_json::json!({"provider":"Codex","live":true}),
                    )
                    .is_err()
            );
            assert_eq!(
                world
                    .store
                    .get_session(world.epic_id)
                    .unwrap()
                    .unwrap()
                    .lead_session_id,
                Some(world.predecessor_id),
                "lead changed at {fault:?}"
            );
            assert_eq!(
                world
                    .store
                    .get_agent_successor(launching.reservation_id)
                    .unwrap()
                    .unwrap()
                    .state,
                AgentSuccessorStateV1::Launching,
                "aggregate changed at {fault:?}"
            );
            let transitions: i64 = world
                .store
                .conn
                .query_row(
                    "SELECT count(*) FROM agent_successor_transitions WHERE reservation_id=?1",
                    [launching.reservation_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(transitions, 2, "audit changed at {fault:?}");
        }
    }

    #[test]
    fn successor_identical_concurrent_reserves_converge() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("successor-concurrency.sqlite");
        let world = world_at(Some(&database));
        let predecessor = world.predecessor_id;
        let request = world.request.clone();
        drop(world.store);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let stores = (0..8)
            .map(|_| Store::open(&database).unwrap())
            .collect::<Vec<_>>();
        let handles = stores
            .into_iter()
            .map(|store| {
                let request = request.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.reserve_agent_successor(
                        predecessor,
                        &request,
                        AgentSuccessorReservationIds {
                            reservation_id: Uuid::new_v4(),
                            candidate_session_id: Uuid::new_v4(),
                            transition_id: Uuid::new_v4(),
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .collect::<Vec<_>>();
        let ids = outcomes
            .iter()
            .map(|outcome| match outcome {
                ReserveAgentSuccessorOutcome::Reserved(record)
                | ReserveAgentSuccessorOutcome::Replayed(record) => record.reservation_id,
            })
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ReserveAgentSuccessorOutcome::Reserved(_)))
                .count(),
            1
        );
    }

    #[test]
    fn successor_changed_fingerprint_concurrent_reserves_converge_to_one_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .join("successor-changed-concurrency.sqlite");
        let world = world_at(Some(&database));
        let predecessor = world.predecessor_id;
        let requests = (0..4)
            .map(|variant| {
                let mut request = world.request.clone();
                request.query = format!("continue the master program variant {variant}");
                request
            })
            .collect::<Vec<_>>();
        let fingerprints = requests
            .iter()
            .map(|request| {
                let request_json = serde_json::to_string(request).unwrap();
                digest(&["agent.successor.request.v1", &request_json])
            })
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(fingerprints.len(), requests.len());
        drop(world.store);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(requests.len()));
        let handles = requests
            .into_iter()
            .map(|request| {
                let store = Store::open(&database).unwrap();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let request_json = serde_json::to_string(&request).unwrap();
                    let fingerprint = digest(&["agent.successor.request.v1", &request_json]);
                    let ids = AgentSuccessorReservationIds {
                        reservation_id: Uuid::new_v4(),
                        candidate_session_id: Uuid::new_v4(),
                        transition_id: Uuid::new_v4(),
                    };
                    barrier.wait();
                    let outcome = store.reserve_agent_successor(predecessor, &request, ids);
                    (fingerprint, ids, outcome)
                })
            })
            .collect::<Vec<_>>();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        let winners = outcomes
            .iter()
            .filter_map(|(fingerprint, ids, outcome)| match outcome {
                Ok(ReserveAgentSuccessorOutcome::Reserved(record)) => {
                    Some((fingerprint, ids, record))
                }
                Ok(ReserveAgentSuccessorOutcome::Replayed(_)) | Err(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(winners.len(), 1, "race allocated more than one identity");
        let (winner_fingerprint, winner_ids, winner) = winners[0];
        assert_eq!(winner.reservation_id, winner_ids.reservation_id);
        assert_eq!(winner.candidate_session_id, winner_ids.candidate_session_id);

        let mut conflict_fingerprints = std::collections::HashSet::new();
        for (fingerprint, _, outcome) in &outcomes {
            match outcome {
                Ok(ReserveAgentSuccessorOutcome::Reserved(record)) => {
                    assert_eq!(record.reservation_id, winner.reservation_id);
                }
                Ok(ReserveAgentSuccessorOutcome::Replayed(_)) => {
                    panic!("distinct request fingerprint replayed the winner")
                }
                Err(DaemonError::InvalidParam(message)) => {
                    assert_ne!(fingerprint, winner_fingerprint);
                    assert_eq!(
                        message,
                        &format!(
                            "agent_successor_idempotency_conflict:{}",
                            winner.reservation_id
                        )
                    );
                    conflict_fingerprints.insert(fingerprint);
                }
                Err(error) => panic!("losing fingerprint returned {error:?}"),
            }
        }
        assert_eq!(conflict_fingerprints.len(), outcomes.len() - 1);

        let snapshot = |store: &Store| {
            let aggregate_count: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM agent_successor_reservations",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let record = store
                .get_agent_successor(winner.reservation_id)
                .unwrap()
                .unwrap();
            let transitions = {
                let mut statement = store
                    .conn
                    .prepare(
                        "SELECT transition_id,reservation_id,state_version,from_state,to_state,
                                authority_digest,launch_attempt_id,model_invocation_id,reason,created_at
                         FROM agent_successor_transitions ORDER BY state_version",
                    )
                    .unwrap();
                statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, Option<String>>(7)?,
                            row.get::<_, Option<String>>(8)?,
                            row.get::<_, String>(9)?,
                        ))
                    })
                    .unwrap()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap()
            };
            (aggregate_count, record, transitions)
        };

        let store = Store::open(&database).unwrap();
        let before_reopen = snapshot(&store);
        assert_eq!(before_reopen.0, 1, "race wrote multiple aggregates");
        assert_eq!(before_reopen.1.reservation_id, winner.reservation_id);
        assert_eq!(
            before_reopen.1.candidate_session_id,
            winner.candidate_session_id
        );
        assert_eq!(before_reopen.1.request_fingerprint, *winner_fingerprint);
        assert_eq!(before_reopen.2.len(), 1, "race wrote extra transitions");
        assert_eq!(before_reopen.2[0].0, winner_ids.transition_id.to_string());
        assert_eq!(before_reopen.2[0].1, winner.reservation_id.to_string());
        assert_eq!(before_reopen.2[0].2, 1);
        assert_eq!(before_reopen.2[0].3, None);
        assert_eq!(before_reopen.2[0].4, "reserved");
        drop(store);

        let reopened = Store::open(&database).unwrap();
        let after_reopen = snapshot(&reopened);
        assert_eq!(after_reopen, before_reopen);
    }
}
