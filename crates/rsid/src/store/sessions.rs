//! Session persistence operations.

use super::row_mappers::{
    SESSION_COLUMNS, capability_class_to_str, map_session_row, sandbox_cleanup_state_to_str,
    sandbox_kind_to_str, session_kind_to_str, session_provider_to_str, session_status_to_str,
    str_to_session_status,
};
use super::successor_reservations::{
    reject_agent_successor_predecessor_continuation_on,
    reject_nonterminal_agent_successor_epic_lead_mutation_on,
};
use super::{STARTUP_PROVIDER_CANDIDATE_MAX, Store};
use crate::error::{DaemonError, Result};
use rsi_common::provider_capabilities::{
    CapabilityConfidence, CapabilitySource, ResolvedContextBudget,
};
use rsi_common::types::{
    SandboxCleanupState, SandboxKind, Session, SessionKind, SessionStatus, is_leaf_kind,
    legal_children,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub(crate) const MAX_OWNING_EPIC_HIERARCHY_DEPTH: usize = 64;

/// Shared historical restore gate. Call through the current transaction so
/// settlement and custody checks remain atomic with the status transition.
pub(super) fn historical_session_restore_blocked_on(conn: &Connection, id: Uuid) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT
            EXISTS(SELECT 1 FROM sessions
                   WHERE id=?1 AND sandbox_cleanup_state='Purged')
            OR EXISTS(
                SELECT 1 FROM source_worktree_settlement_items i
                WHERE i.phase NOT IN ('refused','unattempted')
                  AND (i.session_id=?1 OR i.custody_id IN (
                      SELECT sandbox_custody_id FROM sessions
                          WHERE id=?1 AND sandbox_custody_id IS NOT NULL
                      UNION
                      SELECT custody_id FROM session_execution_projections
                          WHERE session_id=?1 AND custody_id IS NOT NULL
                  )))",
        [id.to_string()],
        |row| row.get(0),
    )?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwningEpicTopologyError {
    MissingOwningEpic,
    Cycle,
    MissingAncestor,
    IllegalEdge,
    MultipleOwningEpics,
    DepthExceeded,
}

/// Resolve exactly one owning Epic from persisted rows. Both guarded Issue
/// authority and successor reservation use this single topology algorithm;
/// their adapters deliberately retain their own public error vocabulary.
pub(crate) fn resolve_owning_epic_topology_tx(
    tx: &Transaction<'_>,
    caller: &Session,
) -> std::result::Result<Session, OwningEpicTopologyError> {
    let mut parent = caller.parent_id;
    let mut child_kind = caller.session_kind;
    let mut found = None;
    let mut seen = std::collections::HashSet::new();
    let mut illegal_edge = false;
    for _ in 0..MAX_OWNING_EPIC_HIERARCHY_DEPTH {
        let Some(parent_id) = parent else {
            if illegal_edge {
                return Err(OwningEpicTopologyError::IllegalEdge);
            }
            return found.ok_or(OwningEpicTopologyError::MissingOwningEpic);
        };
        if !seen.insert(parent_id) {
            return Err(OwningEpicTopologyError::Cycle);
        }
        let ancestor = tx
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id=?1"),
                [parent_id.to_string()],
                map_session_row,
            )
            .optional()
            .map_err(|_| OwningEpicTopologyError::MissingAncestor)?
            .map(super::row_mappers::SessionRow::into_session)
            .transpose()
            .map_err(|_| OwningEpicTopologyError::MissingAncestor)?
            .ok_or(OwningEpicTopologyError::MissingAncestor)?;
        if ancestor.session_kind == SessionKind::Epic {
            if found.is_some() {
                return Err(OwningEpicTopologyError::MultipleOwningEpics);
            }
            found = Some(ancestor.clone());
        }
        illegal_edge |= !legal_children(Some(ancestor.session_kind)).contains(&child_kind);
        parent = ancestor.parent_id;
        child_kind = ancestor.session_kind;
    }
    Err(OwningEpicTopologyError::DepthExceeded)
}

const ARCHIVED_SESSION_LOOKBACK_DAYS: i64 = 7;

impl Store {
    /// Trusted, currently verified sandbox worktrees for codegraph indexing.
    /// Custody IDs survive session rotation; session IDs and branch names do not.
    pub fn list_codegraph_sandbox_registrations(&self) -> Result<Vec<(Uuid, PathBuf, PathBuf)>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.custody_id, r.canonical_repo_dir, r.sandbox_root
             FROM sandbox_custody_roots r
             JOIN sessions s ON s.id=r.owner_session_id
               AND s.sandbox_custody_id=r.custody_id
             WHERE r.state='live' AND r.validation_state='verified'
               AND r.validated_generation=r.generation
               AND s.sandbox_cleanup_state='Live'
               AND s.sandbox_root=r.sandbox_root
             ORDER BY r.custody_id LIMIT 129",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let repo: String = row.get(1)?;
                let root: String = row.get(2)?;
                Ok((id, repo, root))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(id, repo, root)| {
                let id = Uuid::parse_str(&id).map_err(|error| {
                    DaemonError::Store(format!("invalid codegraph custody UUID: {error}"))
                })?;
                Ok((id, PathBuf::from(repo), PathBuf::from(root)))
            })
            .collect()
    }

    /// Insert a new session (including metadata columns and project_id).
    pub fn insert_session(&self, session: &Session) -> Result<()> {
        insert_session_on(&self.conn, session)
    }

    /// Insert a new session row and bind it to the admitted model invocation
    /// before any asynchronous finalization can observe the session.
    pub fn insert_session_with_model_invocation(
        &self,
        session: &Session,
        invocation_id: Uuid,
    ) -> Result<()> {
        self.insert_session(session)?;
        self.set_session_model_invocation(session.id, Some(invocation_id))
    }

    /// Return whether `caller_session_id` is the durable lead recorded on the
    /// parent that directly owns `child_session_id`.
    ///
    /// Status/halt historically accept any parent carrying this pointer; this
    /// predicate preserves that scope without consulting runtime caches. A
    /// committed successor baton changes SQLite before runtime projection
    /// publication, so consulting a resident parent during that interval
    /// would create a second authority plane.
    pub(crate) fn parent_lead_authorizes_child(
        &self,
        caller_session_id: Uuid,
        child_session_id: Uuid,
    ) -> Result<bool> {
        let authorized = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1
                   FROM sessions AS child
                   JOIN sessions AS parent ON parent.id=child.parent_id
                  WHERE child.id=?1
                    AND parent.lead_session_id=?2
             )",
            params![child_session_id.to_string(), caller_session_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(authorized)
    }

    /// Messaging uses the narrower durable authority contract: the parent
    /// carrying the caller's lead pointer must be exactly an Epic.
    pub(crate) fn epic_lead_authorizes_child(
        &self,
        caller_session_id: Uuid,
        child_session_id: Uuid,
    ) -> Result<bool> {
        let authorized = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1
                   FROM sessions AS child
                   JOIN sessions AS epic ON epic.id=child.parent_id
                  WHERE child.id=?1
                    AND epic.session_kind='Epic'
                    AND epic.lead_session_id=?2
             )",
            params![child_session_id.to_string(), caller_session_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(authorized)
    }

    /// Resolve the persisted owning-Epic authority used by V97 Issue control.
    /// This walks only durable rows and deliberately verifies every parent edge
    /// plus lead generation inside the caller's surrounding transaction.
    pub(crate) fn resolve_agent_issue_authority_tx(
        tx: &Transaction<'_>,
        caller_session_id: Uuid,
    ) -> Result<super::issues::AgentIssueAuthority> {
        let caller = tx
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id=?1"),
                [caller_session_id.to_string()],
                map_session_row,
            )
            .optional()?
            .map(super::row_mappers::SessionRow::into_session)
            .transpose()?
            .ok_or_else(|| DaemonError::PolicyDenied("agent_issue_authority_denied".to_string()))?;
        if !is_leaf_kind(caller.session_kind) {
            return Err(DaemonError::PolicyDenied(
                "agent_issue_authority_denied".to_string(),
            ));
        }
        let caller_project = caller
            .project_id
            .ok_or_else(|| DaemonError::PolicyDenied("agent_issue_authority_denied".to_string()))?;
        let epic = resolve_owning_epic_topology_tx(tx, &caller)
            .map_err(|_| DaemonError::PolicyDenied("agent_issue_authority_denied".to_string()))?;
        if epic.project_id != Some(caller_project)
            || epic.lead_session_id != Some(caller_session_id)
        {
            return Err(DaemonError::PolicyDenied(
                "agent_issue_authority_denied".to_string(),
            ));
        }
        let generation: i64 = tx
            .query_row(
                "SELECT generation FROM epic_lead_generations WHERE epic_id=?1",
                [epic.id.to_string()],
                |row| row.get(0),
            )
            .map_err(|_| DaemonError::PolicyDenied("agent_issue_authority_denied".to_string()))?;
        if generation < 1 {
            return Err(DaemonError::PolicyDenied(
                "agent_issue_authority_denied".to_string(),
            ));
        }
        Ok(super::issues::AgentIssueAuthority {
            caller_session_id,
            project_id: caller_project,
            actor: super::issues::AgentIssueActor::Lead {
                epic_id: epic.id,
                lead_generation: generation,
            },
        })
    }
}

/// Fixture-only twin of [`insert_session_on`] that writes the **frozen**
/// base-schema `sessions` column set.
///
/// [`insert_session_on`] names the current catalog unconditionally and must stay
/// that way — a deployed store missing a current column has to fail closed
/// rather than silently write a stale row shape. But a migration-chain fixture
/// deliberately runs *below* the head, so seeding it through the production
/// insert makes that fixture depend on no future migration ever adding a
/// `sessions` column.
///
/// V99 was the first `sessions`-widening migration since those fixtures were
/// written and it broke exactly that way, in 28 tests, all with
/// `table sessions has no column named provider_cli_version`. This helper is the
/// same remedy `insert_session_with_pre_v98_custody` already applies to
/// `sandbox_custody_roots`: fixtures select a frozen shape.
///
/// Only the columns the original `sessions` DDL declares are listed. Every
/// column added after it is nullable or carries a DEFAULT, so this insert is
/// valid at every schema version from the base forward — which means the *next*
/// `sessions` migration needs to touch none of these fixtures.
#[cfg(test)]
pub(super) fn insert_legacy_session_on(connection: &Connection, session: &Session) -> Result<()> {
    connection.execute(
        "INSERT INTO sessions (id, claude_session_id, provider, query, working_dir,
         status, created_at, updated_at, session_kind, model,
         sandbox_kind, sandbox_root, sandbox_branch, sandbox_cleanup_state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            session.id.to_string(),
            session.claude_session_id,
            super::row_mappers::session_provider_to_str(session.provider),
            session.query,
            session.working_dir.to_string_lossy().to_string(),
            super::row_mappers::session_status_to_str(session.status),
            session.created_at.to_rfc3339(),
            session.updated_at.to_rfc3339(),
            super::row_mappers::session_kind_to_str(session.session_kind),
            session.model,
            session
                .sandbox_kind
                .map(super::row_mappers::sandbox_kind_to_str),
            session
                .sandbox_root
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            session.sandbox_branch,
            session
                .sandbox_cleanup_state
                .map(super::row_mappers::sandbox_cleanup_state_to_str),
        ],
    )?;
    Ok(())
}

/// Shared session-row insert used by ordinary writes and compound
/// coordination transactions. `rusqlite::Transaction` dereferences to its
/// connection, so callers keep the canonical serialization in one place.
pub(super) fn insert_session_on(connection: &Connection, session: &Session) -> Result<()> {
    let pending_question_json = session
        .pending_question
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| DaemonError::Store(format!("Invalid pending question JSON: {}", e)))?;
    let context_budget = persisted_context_budget(
        session.context_window,
        session.resolved_context_budget.as_ref(),
    )?;

    // An empty capability set is stored as NULL rather than "[]": both decode
    // to an empty vec, and NULL keeps "never observed" indistinguishable from
    // pre-V99 rows instead of implying the CLI advertised nothing.
    let provider_capabilities_json = if session.provider_capabilities.is_empty() {
        None
    } else {
        Some(
            serde_json::to_string(&session.provider_capabilities).map_err(|e| {
                DaemonError::Store(format!("Invalid provider capabilities JSON: {}", e))
            })?,
        )
    };

    connection.execute(
            "INSERT INTO sessions (id, claude_session_id, provider, query, working_dir, status,
             project_id, pinned_at, created_at, updated_at, cost_usd, duration_ms, num_turns, model,
             input_tokens, output_tokens, context_window,
             total_input_tokens, total_output_tokens, total_cache_creation_tokens, total_cache_read_tokens,
             stop_reason, session_kind, continued_from, handoff_filepath, rotation_depth,
             daemon_input_tokens, daemon_output_tokens, title, description, pipeline_artifact, workflow_id,
             git_branch, active_task, group_id, pending_archive, effort, retry_attempt, max_retries,
             issue_identifier, issue_url, issue_tracker_id, scheduled_job_id,
             rating, harness_version_hash, test_passed, clippy_passed, turn_count, retry_count,
             sandbox_kind, sandbox_root, sandbox_branch, sandbox_cleanup_state, parent_id,
             approval_wait_ms, lead_session_id, is_eval, capability_class,
             topology_node_id, topology_iteration, pending_question_json, work_time_ms,
             testing_needed_at, rotation_disabled_at,
             provider_cli_version, provider_capabilities, thinking_tokens, service_tier,
             cache_creation_1h_tokens, cache_creation_5m_tokens, permission_denial_count,
             subagent_stats_json, queued_turn_count, terminal_reason,
             context_window_source, context_window_source_version, context_window_source_digest,
             context_window_observed_at, context_window_configured_tokens,
             agent_role, epic_spawn_ordinal)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                     ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32,
                     ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43,
                     ?44, ?45, ?46, ?47, ?48, ?49,
                     ?50, ?51, ?52, ?53, ?54, ?55, ?56, ?57, ?58, ?59, ?60, ?61, ?62, ?63, ?64,
                     ?65, ?66, ?67, ?68, ?69, ?70, ?71, ?72, ?73, ?74, ?75, ?76, ?77, ?78, ?79,
                     ?80, ?81)",
            params![
                session.id.to_string(),
                session.claude_session_id,
                session_provider_to_str(session.provider),
                session.query,
                session.working_dir.to_string_lossy().to_string(),
                session_status_to_str(session.status),
                session.project_id.map(|id| id.to_string()),
                session.pinned_at.map(|dt| dt.to_rfc3339()),
                session.created_at.to_rfc3339(),
                session.updated_at.to_rfc3339(),
                session.cost_usd,
                session.duration_ms.map(|v| v as i64),
                session.num_turns.map(|v| v as i32),
                session.model,
                session.input_tokens.map(|v| v as i64),
                session.output_tokens.map(|v| v as i64),
                context_budget.context_window,
                session.total_input_tokens.map(|v| v as i64),
                session.total_output_tokens.map(|v| v as i64),
                session.total_cache_creation_tokens.map(|v| v as i64),
                session.total_cache_read_tokens.map(|v| v as i64),
                session.stop_reason.clone(),
                session_kind_to_str(session.session_kind),
                session.continued_from.map(|id| id.to_string()),
                session.handoff_filepath.clone(),
                session.rotation_depth as i64,
                session.daemon_input_tokens.map(|v| v as i64),
                session.daemon_output_tokens.map(|v| v as i64),
                session.title.as_deref(),
                session.description.as_deref(),
                session.pipeline_artifact.as_deref(),
                session.workflow_id.map(|id| id.to_string()),
                session.git_branch.as_deref(),
                session.active_task.as_deref(),
                session.group_id.map(|id| id.to_string()),
                session.pending_archive as i64,
                session.effort.as_deref(),
                session.retry_attempt.map(|v| v as i64),
                session.max_retries.map(|v| v as i64),
                session.issue_identifier.as_deref(),
                session.issue_url.as_deref(),
                session.issue_tracker_id.as_deref(),
                session.scheduled_job_id.map(|id| id.to_string()),
                session.rating.map(|v| v as i64),
                session.harness_version_hash.as_deref(),
                session.test_passed.map(|v| v as i64),
                session.clippy_passed.map(|v| v as i64),
                session.turn_count.map(|v| v as i64),
                session.retry_count.map(|v| v as i64),
                session.sandbox_kind.map(sandbox_kind_to_str),
                session.sandbox_root.as_ref().map(|p| p.to_string_lossy().to_string()),
                session.sandbox_branch.as_deref(),
                session.sandbox_cleanup_state.map(sandbox_cleanup_state_to_str),
                session.parent_id.map(|id| id.to_string()),
                session.approval_wait_ms.map(|v| v as i64),
                session.lead_session_id.map(|id| id.to_string()),
                i64::from(session.is_eval),
                session.capability_class.map(capability_class_to_str),
                session.topology_node_id.as_deref(),
                session.topology_iteration as i64,
                pending_question_json.as_deref(),
                session.work_time_ms.map(|v| v as i64),
                session.testing_needed_at.map(|value| {
                    value.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                }),
                session.rotation_disabled_at.map(|value| {
                    value.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                }),
                session.provider_cli_version.as_deref(),
                provider_capabilities_json.as_deref(),
                session.thinking_tokens.map(|v| v as i64),
                session.service_tier.as_deref(),
                session.cache_creation_1h_tokens.map(|v| v as i64),
                session.cache_creation_5m_tokens.map(|v| v as i64),
                session.permission_denial_count.map(|v| v as i64),
                session.subagent_stats_json.as_deref(),
                session.queued_turn_count.map(|v| v as i64),
                session.terminal_reason.as_deref(),
                context_budget.source,
                context_budget.source_version,
                context_budget.source_digest,
                context_budget.observed_at,
                context_budget.configured_tokens,
                session.agent_role.as_deref(),
                session.epic_spawn_ordinal.map(i64::from),
            ],
        )?;
    Ok(())
}

/// Frozen pre-V99 Session row shape for migration fixtures. Production writes
/// always use [`insert_session_on`] and therefore refuse a stale catalog.
#[cfg(test)]
pub(super) fn insert_pre_v99_session_on(connection: &Connection, session: &Session) -> Result<()> {
    let pending_question_json = session
        .pending_question
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| DaemonError::Store(format!("Invalid pending question JSON: {}", e)))?;
    connection.execute(
        "INSERT INTO sessions (id, claude_session_id, provider, query, working_dir, status,
         project_id, pinned_at, created_at, updated_at, cost_usd, duration_ms, num_turns, model,
         input_tokens, output_tokens, context_window,
         total_input_tokens, total_output_tokens, total_cache_creation_tokens, total_cache_read_tokens,
         stop_reason, session_kind, continued_from, handoff_filepath, rotation_depth,
         daemon_input_tokens, daemon_output_tokens, title, description, pipeline_artifact, workflow_id,
         git_branch, active_task, group_id, pending_archive, effort, retry_attempt, max_retries,
         issue_identifier, issue_url, issue_tracker_id, scheduled_job_id,
         rating, harness_version_hash, test_passed, clippy_passed, turn_count, retry_count,
         sandbox_kind, sandbox_root, sandbox_branch, sandbox_cleanup_state, parent_id,
         approval_wait_ms, lead_session_id, is_eval, capability_class,
         topology_node_id, topology_iteration, pending_question_json, work_time_ms,
         testing_needed_at, rotation_disabled_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32,
                 ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43,
                 ?44, ?45, ?46, ?47, ?48, ?49,
                 ?50, ?51, ?52, ?53, ?54, ?55, ?56, ?57, ?58, ?59, ?60, ?61, ?62, ?63, ?64)",
        params![
            session.id.to_string(),
            session.claude_session_id,
            session_provider_to_str(session.provider),
            session.query,
            session.working_dir.to_string_lossy().to_string(),
            session_status_to_str(session.status),
            session.project_id.map(|id| id.to_string()),
            session.pinned_at.map(|dt| dt.to_rfc3339()),
            session.created_at.to_rfc3339(),
            session.updated_at.to_rfc3339(),
            session.cost_usd,
            session.duration_ms.map(|v| v as i64),
            session.num_turns.map(|v| v as i32),
            session.model,
            session.input_tokens.map(|v| v as i64),
            session.output_tokens.map(|v| v as i64),
            session.context_window.map(|v| v as i64),
            session.total_input_tokens.map(|v| v as i64),
            session.total_output_tokens.map(|v| v as i64),
            session.total_cache_creation_tokens.map(|v| v as i64),
            session.total_cache_read_tokens.map(|v| v as i64),
            session.stop_reason.clone(),
            session_kind_to_str(session.session_kind),
            session.continued_from.map(|id| id.to_string()),
            session.handoff_filepath.clone(),
            session.rotation_depth as i64,
            session.daemon_input_tokens.map(|v| v as i64),
            session.daemon_output_tokens.map(|v| v as i64),
            session.title.as_deref(),
            session.description.as_deref(),
            session.pipeline_artifact.as_deref(),
            session.workflow_id.map(|id| id.to_string()),
            session.git_branch.as_deref(),
            session.active_task.as_deref(),
            session.group_id.map(|id| id.to_string()),
            session.pending_archive as i64,
            session.effort.as_deref(),
            session.retry_attempt.map(|v| v as i64),
            session.max_retries.map(|v| v as i64),
            session.issue_identifier.as_deref(),
            session.issue_url.as_deref(),
            session.issue_tracker_id.as_deref(),
            session.scheduled_job_id.map(|id| id.to_string()),
            session.rating.map(|v| v as i64),
            session.harness_version_hash.as_deref(),
            session.test_passed.map(|v| v as i64),
            session.clippy_passed.map(|v| v as i64),
            session.turn_count.map(|v| v as i64),
            session.retry_count.map(|v| v as i64),
            session.sandbox_kind.map(sandbox_kind_to_str),
            session
                .sandbox_root
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            session.sandbox_branch.as_deref(),
            session
                .sandbox_cleanup_state
                .map(sandbox_cleanup_state_to_str),
            session.parent_id.map(|id| id.to_string()),
            session.approval_wait_ms.map(|v| v as i64),
            session.lead_session_id.map(|id| id.to_string()),
            i64::from(session.is_eval),
            session.capability_class.map(capability_class_to_str),
            session.topology_node_id.as_deref(),
            session.topology_iteration as i64,
            pending_question_json.as_deref(),
            session.work_time_ms.map(|v| v as i64),
            session
                .testing_needed_at
                .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
            session
                .rotation_disabled_at
                .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
        ],
    )?;
    Ok(())
}

pub(super) struct PersistedContextBudget<'a> {
    pub context_window: Option<i64>,
    pub configured_tokens: Option<i64>,
    pub source: Option<&'static str>,
    pub source_version: Option<&'a str>,
    pub source_digest: Option<&'a str>,
    pub observed_at: Option<String>,
}

pub(super) fn persisted_context_budget<'a>(
    context_window: Option<u64>,
    resolved: Option<&'a ResolvedContextBudget>,
) -> Result<PersistedContextBudget<'a>> {
    if context_window == Some(0) {
        return Err(DaemonError::InvalidParam(
            "context window must be greater than zero".into(),
        ));
    }
    let context_window_i64 = context_window
        .map(i64::try_from)
        .transpose()
        .map_err(|_| DaemonError::InvalidParam("context window exceeds SQLite INTEGER".into()))?;
    let Some(resolved) = resolved else {
        return Ok(PersistedContextBudget {
            context_window: context_window_i64,
            configured_tokens: None,
            source: context_window.map(|_| CapabilitySource::LegacyUnverified.as_str()),
            source_version: None,
            source_digest: None,
            observed_at: None,
        });
    };
    let Some(context_window) = context_window else {
        return Err(DaemonError::InvalidParam(
            "resolved context budget requires context_window".into(),
        ));
    };
    if resolved.active_tokens == 0 || resolved.active_tokens != context_window {
        return Err(DaemonError::InvalidParam(
            "resolved context budget active_tokens must equal context_window".into(),
        ));
    }
    let expected_confidence = match resolved.evidence.source {
        CapabilitySource::RuntimeTelemetry | CapabilitySource::Configured => {
            CapabilityConfidence::Authoritative
        }
        CapabilitySource::OfficialDocumentation | CapabilitySource::ProviderCatalog => {
            CapabilityConfidence::Verified
        }
        CapabilitySource::RepositoryFallback | CapabilitySource::LegacyUnverified => {
            CapabilityConfidence::Degraded
        }
    };
    if resolved.evidence.confidence != expected_confidence {
        return Err(DaemonError::InvalidParam(
            "context budget confidence is incoherent with its source".into(),
        ));
    }
    let configured_tokens = resolved
        .capacity
        .configured_tokens
        .map(i64::try_from)
        .transpose()
        .map_err(|_| {
            DaemonError::InvalidParam("configured context window exceeds SQLite INTEGER".into())
        })?;
    if configured_tokens == Some(0) {
        return Err(DaemonError::InvalidParam(
            "configured context window must be greater than zero".into(),
        ));
    }
    // Raw configured intent is a descriptive capacity fact. It may coexist
    // with degraded active-budget provenance while a native provider's
    // effective factor or runtime telemetry is still unavailable.
    Ok(PersistedContextBudget {
        context_window: context_window_i64,
        configured_tokens,
        source: Some(resolved.evidence.source.as_str()),
        source_version: resolved.evidence.source_version.as_deref(),
        source_digest: resolved.evidence.source_digest.as_deref(),
        observed_at: resolved
            .evidence
            .observed_at
            .as_ref()
            .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
    })
}

impl Store {
    /// List every session row whose `sandbox_cleanup_state = 'Live'` along
    /// with the owning session's status and sandbox_root path. The startup
    /// orphan sweep uses this inventory only to classify and retain candidates;
    /// absence from the list is never deletion authority.
    ///
    /// Rows whose `sandbox_root` column is NULL are skipped — the Live state
    /// without a root is a degenerate case that shouldn't exist but is
    /// harmless to ignore.
    pub fn list_live_sandbox_owners(&self) -> Result<Vec<(Uuid, SessionStatus, PathBuf)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, status, sandbox_root \
             FROM sessions \
             WHERE sandbox_cleanup_state = 'Live' AND sandbox_root IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id_str: String = row.get(0)?;
                let status_str: String = row.get(1)?;
                let root_str: String = row.get(2)?;
                Ok((id_str, status_str, root_str))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut out = Vec::with_capacity(rows.len());
        for (id_str, status_str, root_str) in rows {
            let id = Uuid::parse_str(&id_str).map_err(|e| {
                DaemonError::Store(format!("invalid session UUID in sessions table: {}", e))
            })?;
            let status = str_to_session_status(&status_str)?;
            out.push((id, status, PathBuf::from(root_str)));
        }
        Ok(out)
    }

    /// List build-cache reclamation candidates (issue #25): sessions with a
    /// Live sandbox whose status is terminal (no provider process can be
    /// writing into the sandbox), along with the row's `updated_at` so the
    /// caller can age-gate the reclaim. Statuses `Starting`, `Running`, and
    /// `WaitingApproval` are excluded — their sandboxes are in active use.
    ///
    /// This returns candidates only; the caller owns every filesystem-level
    /// safety check (containment under the sandbox base dir, UUID-named root,
    /// non-symlink cache dir) before deleting anything.
    pub fn list_terminal_sandbox_build_cache_owners(&self) -> Result<Vec<(Uuid, PathBuf, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, sandbox_root, updated_at \
             FROM sessions \
             WHERE sandbox_cleanup_state = 'Live' AND sandbox_root IS NOT NULL \
               AND status IN ('Completed','Failed','Interrupted','Archived','Deleted')",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id_str: String = row.get(0)?;
                let root_str: String = row.get(1)?;
                let updated_at: String = row.get(2)?;
                Ok((id_str, root_str, updated_at))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut out = Vec::with_capacity(rows.len());
        for (id_str, root_str, updated_at) in rows {
            let id = Uuid::parse_str(&id_str).map_err(|e| {
                DaemonError::Store(format!("invalid session UUID in sessions table: {}", e))
            })?;
            out.push((id, PathBuf::from(root_str), updated_at));
        }
        Ok(out)
    }

    /// Update a session's sandbox cleanup state. None clears the column.
    pub fn update_sandbox_cleanup_state(
        &self,
        session_id: Uuid,
        state: Option<SandboxCleanupState>,
    ) -> Result<()> {
        self.reject_custody_linked_sandbox_mutation(session_id)?;
        self.conn.execute(
            "UPDATE sessions SET sandbox_cleanup_state = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                state.map(sandbox_cleanup_state_to_str),
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Atomically tombstone a session's sandbox metadata after a successful
    /// teardown: clear `sandbox_root` and `sandbox_branch`, and stamp
    /// `sandbox_cleanup_state = 'Purged'`. `sandbox_kind` is preserved so
    /// the historical record of "this session WAS sandboxed" remains queryable.
    ///
    /// This prevents the failure mode where a stale `sandbox_root` path
    /// (whose worktree was destroyed) is later read by `continue_session` /
    /// `rotate_session` and handed to `Command::current_dir`, producing a
    /// `chdir(2)` ENOENT at spawn time.
    pub fn mark_sandbox_purged(&self, session_id: Uuid) -> Result<()> {
        self.reject_custody_linked_sandbox_mutation(session_id)?;
        self.conn.execute(
            "UPDATE sessions
             SET sandbox_cleanup_state = ?1,
                 sandbox_root = NULL,
                 sandbox_branch = NULL,
                 updated_at = ?2
             WHERE id = ?3",
            params![
                sandbox_cleanup_state_to_str(SandboxCleanupState::Purged),
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Re-hydrate a session's sandbox metadata after a fresh sandbox was
    /// allocated for a continue/resume whose prior sandbox had been torn down
    /// (the inverse of [`Store::mark_sandbox_purged`]). Sets `sandbox_root`,
    /// `sandbox_branch`, and `sandbox_kind` to the new allocation, stamps
    /// `sandbox_cleanup_state = 'Live'`, and refreshes `git_branch` to the
    /// new worktree branch (when the allocation created one).
    ///
    /// Persisting here — rather than relying solely on the in-memory
    /// `TrackedSession` — keeps terminal cleanup and a mid-run daemon restart
    /// reconciling against the LIVE worktree path instead of the stale,
    /// already-purged one.
    pub fn restore_sandbox_allocation(
        &self,
        session_id: Uuid,
        kind: SandboxKind,
        root: &Path,
        branch: Option<&str>,
        git_branch: Option<&str>,
    ) -> Result<()> {
        self.reject_custody_linked_sandbox_mutation(session_id)?;
        self.conn.execute(
            "UPDATE sessions
             SET sandbox_kind = ?1,
                 sandbox_root = ?2,
                 sandbox_branch = ?3,
                 sandbox_cleanup_state = ?4,
                 git_branch = COALESCE(?5, git_branch),
                 updated_at = ?6
             WHERE id = ?7",
            params![
                sandbox_kind_to_str(kind),
                root.to_string_lossy().to_string(),
                branch,
                sandbox_cleanup_state_to_str(SandboxCleanupState::Live),
                git_branch,
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// V83-linked rows are an aggregate's history.  Their sandbox tuple may
    /// only change through the root-scoped custody transaction.
    fn reject_custody_linked_sandbox_mutation(&self, session_id: Uuid) -> Result<()> {
        let linked: Option<bool> = self
            .conn
            .query_row(
                "SELECT sandbox_custody_id IS NOT NULL FROM sessions WHERE id=?1",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if linked == Some(true) {
            return Err(DaemonError::Store(
                "sandbox custody-linked session requires root-scoped transition".into(),
            ));
        }
        Ok(())
    }

    /// Update session status and updated_at timestamp.
    pub fn update_session_status(&self, id: Uuid, status: SessionStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                session_status_to_str(status),
                chrono::Utc::now().to_rfc3339(),
                id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Update the serialized pending AskUserQuestion payload. None clears it.
    pub fn update_session_pending_question_json(
        &self,
        session_id: Uuid,
        pending_question_json: Option<&str>,
    ) -> Result<()> {
        // Compatibility setter: preserve display behavior, but every write
        // (including equal text or clear) invalidates producer-bound identity.
        self.reserve_pending_question_publication(session_id, pending_question_json)?;
        Ok(())
    }

    /// Update session_kind (e.g., when escalating TaskRabbit -> Standard).
    pub fn update_session_kind(&self, id: Uuid, kind: SessionKind) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET session_kind = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                session_kind_to_str(kind),
                chrono::Utc::now().to_rfc3339(),
                id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Update claude_session_id when first received from stream.
    pub fn update_claude_session_id(&self, id: Uuid, claude_session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET claude_session_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                claude_session_id,
                chrono::Utc::now().to_rfc3339(),
                id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Persist session metadata (tokens, cost, model, etc.) at session completion.
    pub fn update_session_metadata(&self, session: &Session) -> Result<()> {
        let context_budget = persisted_context_budget(
            session.context_window,
            session.resolved_context_budget.as_ref(),
        )?;
        self.conn.execute(
            "UPDATE sessions SET cost_usd = ?1, duration_ms = ?2, num_turns = ?3,
             model = ?4, input_tokens = ?5, output_tokens = ?6, context_window = ?7,
             total_input_tokens = ?8, total_output_tokens = ?9, total_cache_creation_tokens = ?10,
             total_cache_read_tokens = ?11, stop_reason = ?12, handoff_filepath = ?13,
             daemon_input_tokens = ?14, daemon_output_tokens = ?15, git_branch = ?16, active_task = ?17,
             harness_version_hash = ?18, test_passed = ?19, clippy_passed = ?20,
             turn_count = ?21, retry_count = ?22, approval_wait_ms = ?23,
             work_time_ms = ?24,
             thinking_tokens = ?25, service_tier = ?26, cache_creation_1h_tokens = ?27,
             cache_creation_5m_tokens = ?28, permission_denial_count = ?29,
             subagent_stats_json = ?30, queued_turn_count = ?31, terminal_reason = ?32,
             context_window_source = ?33, context_window_source_version = ?34,
             context_window_source_digest = ?35, context_window_observed_at = ?36,
             context_window_configured_tokens = ?37, pipeline_artifact = ?38,
             session_kind = ?39, updated_at = ?40 WHERE id = ?41",
            params![
                session.cost_usd,
                session.duration_ms.map(|v| v as i64),
                session.num_turns.map(|v| v as i32),
                session.model,
                session.input_tokens.map(|v| v as i64),
                session.output_tokens.map(|v| v as i64),
                context_budget.context_window,
                session.total_input_tokens.map(|v| v as i64),
                session.total_output_tokens.map(|v| v as i64),
                session.total_cache_creation_tokens.map(|v| v as i64),
                session.total_cache_read_tokens.map(|v| v as i64),
                session.stop_reason,
                session.handoff_filepath,
                session.daemon_input_tokens.map(|v| v as i64),
                session.daemon_output_tokens.map(|v| v as i64),
                session.git_branch.as_deref(),
                session.active_task.as_deref(),
                session.harness_version_hash.as_deref(),
                session.test_passed.map(|v| v as i64),
                session.clippy_passed.map(|v| v as i64),
                session.turn_count.map(|v| v as i64),
                session.retry_count.map(|v| v as i64),
                session.approval_wait_ms.map(|v| v as i64),
                session.work_time_ms.map(|v| v as i64),
                session.thinking_tokens.map(|v| v as i64),
                session.service_tier.as_deref(),
                session.cache_creation_1h_tokens.map(|v| v as i64),
                session.cache_creation_5m_tokens.map(|v| v as i64),
                session.permission_denial_count.map(|v| v as i64),
                session.subagent_stats_json.as_deref(),
                session.queued_turn_count.map(|v| v as i64),
                session.terminal_reason.as_deref(),
                context_budget.source,
                context_budget.source_version,
                context_budget.source_digest,
                context_budget.observed_at,
                context_budget.configured_tokens,
                session.pipeline_artifact.as_deref(),
                session_kind_to_str(session.session_kind),
                chrono::Utc::now().to_rfc3339(),
                session.id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Record the provider CLI version and capability set advertised at
    /// `system/init` (V99, P1-A).
    ///
    /// Written on every init rather than latched on the first: a resumed
    /// session re-announces, and the operator may have upgraded the CLI in
    /// between, so the most recent handshake is the truthful one.
    pub fn update_session_provider_handshake(
        &self,
        session_id: Uuid,
        cli_version: Option<&str>,
        capabilities: &[String],
    ) -> Result<()> {
        let capabilities_json = if capabilities.is_empty() {
            None
        } else {
            Some(serde_json::to_string(capabilities).map_err(|e| {
                DaemonError::Store(format!("Invalid provider capabilities JSON: {}", e))
            })?)
        };
        self.conn.execute(
            "UPDATE sessions SET provider_cli_version = ?1, provider_capabilities = ?2,
             updated_at = ?3 WHERE id = ?4",
            params![
                cli_version,
                capabilities_json.as_deref(),
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Update a session's user rating (1–10 scale). None clears the rating.
    /// Rating-range validation is the caller's responsibility.
    pub fn update_session_rating(&self, session_id: Uuid, rating: Option<i16>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET rating = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                rating.map(|v| v as i64),
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Compare-and-swap the persisted model/window/evidence tuple.
    ///
    /// Monitor generations use this to prevent an older provider incarnation
    /// from overwriting a newer incarnation's durable context authority after
    /// an async persistence wait. `Ok(false)` means the expected tuple no
    /// longer owns the row. An identical desired tuple is a successful no-op
    /// and does not rewrite `updated_at`.
    #[allow(clippy::too_many_arguments)]
    pub fn compare_and_update_session_model(
        &self,
        session_id: Uuid,
        expected_model: Option<&str>,
        expected_context_window: Option<u64>,
        expected_resolved_context_budget: Option<&ResolvedContextBudget>,
        model: Option<&str>,
        context_window: Option<u64>,
        resolved_context_budget: Option<&ResolvedContextBudget>,
    ) -> Result<bool> {
        let expected =
            persisted_context_budget(expected_context_window, expected_resolved_context_budget)?;
        let desired = persisted_context_budget(context_window, resolved_context_budget)?;
        let current = self
            .conn
            .query_row(
                "SELECT model, context_window, context_window_source,
                        context_window_source_version, context_window_source_digest,
                        context_window_observed_at, context_window_configured_tokens
                   FROM sessions WHERE id = ?1",
                params![session_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some(current) = current else {
            return Err(DaemonError::SessionNotFound(session_id));
        };
        let expected_tuple = (
            expected_model.map(str::to_owned),
            expected.context_window,
            expected.source.map(str::to_owned),
            expected.source_version.map(str::to_owned),
            expected.source_digest.map(str::to_owned),
            expected.observed_at,
            expected.configured_tokens,
        );
        if current != expected_tuple {
            return Ok(false);
        }
        let desired_tuple = (
            model.map(str::to_owned),
            desired.context_window,
            desired.source.map(str::to_owned),
            desired.source_version.map(str::to_owned),
            desired.source_digest.map(str::to_owned),
            desired.observed_at.clone(),
            desired.configured_tokens,
        );
        if current == desired_tuple {
            return Ok(true);
        }

        self.conn.execute(
            "UPDATE sessions SET model = ?1, context_window = ?2,
             context_window_source = ?3, context_window_source_version = ?4,
             context_window_source_digest = ?5, context_window_observed_at = ?6,
             context_window_configured_tokens = ?7,
             updated_at = ?8 WHERE id = ?9",
            params![
                model,
                desired.context_window,
                desired.source,
                desired.source_version,
                desired.source_digest,
                desired.observed_at,
                desired.configured_tokens,
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                session_id.to_string(),
            ],
        )?;
        Ok(true)
    }

    /// Load all sessions (for daemon startup restore).
    /// Populates `short_summary` from the session_summaries table via batch query.
    pub fn load_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE status != 'Archived' AND status != 'Deleted' ORDER BY created_at ASC"
        ))?;

        let sessions = stmt
            .query_map([], map_session_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut sessions: Vec<Session> = sessions
            .into_iter()
            .map(|row| row.into_session())
            .collect::<Result<Vec<_>>>()?;

        // Batch-populate short_summary from session_summaries table
        let ids: Vec<uuid::Uuid> = sessions.iter().map(|s| s.id).collect();
        if let Ok(summaries) = self.batch_get_short_summaries(&ids) {
            for session in &mut sessions {
                if let Some(summary) = summaries.get(&session.id) {
                    session.short_summary = Some(summary.clone());
                }
            }
        }

        // Hydrate tags from session_tags (P1.5).
        self.hydrate_tags(&mut sessions)?;

        Ok(sessions)
    }

    /// Get a single session by ID.
    /// Populates `short_summary` from the session_summaries table.
    pub fn get_session(&self, id: Uuid) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"
        ))?;

        let mut rows = stmt.query_map(params![id.to_string()], map_session_row)?;

        match rows.next() {
            Some(Ok(row)) => {
                let mut session = row.into_session()?;
                // Populate short_summary from session_summaries table
                if let Ok(Some(content)) = self.get_short_summary_content(id) {
                    session.short_summary = Some(content);
                }
                // Hydrate tags from session_tags (P1.5).
                let mut sessions = vec![session];
                self.hydrate_tags(&mut sessions)?;
                session = sessions.into_iter().next().unwrap();
                Ok(Some(session))
            }
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// A8: most recent rotation child of `id` (`continued_from = id`) — the
    /// next hop when chasing a wake target's rotation lineage to its live
    /// tip. Read-only; rides `idx_sessions_continued_from`. No schema change.
    ///
    /// # Errors
    /// Fails on `SQLite` errors or a malformed UUID in the successor row.
    pub fn find_rotation_successor(&self, id: Uuid) -> Result<Option<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM sessions WHERE continued_from = ?1 \
             ORDER BY created_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![id.to_string()], |row| row.get::<_, String>(0))?;
        match rows.next() {
            Some(Ok(s)) => Ok(Some(
                Uuid::parse_str(&s).map_err(|e| DaemonError::Store(e.to_string()))?,
            )),
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// Soft-delete a session: set status to Deleted. Data is preserved for trash browser.
    pub fn soft_delete_session(&self, id: Uuid) -> Result<()> {
        self.update_session_status(id, SessionStatus::Deleted)?;
        Ok(())
    }

    /// Hard-delete a session and its ordinary child data while retaining the
    /// terminal execution projection as immutable audit/custody history.
    /// Deletes in dependency order to satisfy foreign key constraints.
    /// Use this to permanently purge a session from the trash.
    #[allow(dead_code)]
    pub fn purge_session(&self, id: Uuid) -> Result<()> {
        let id_str = id.to_string();
        let tx = self.conn.unchecked_transaction()?;
        if !super::sandbox_custody::prepare_session_execution_projection_for_purge(&tx, id)? {
            return Ok(());
        }
        tx.execute(
            "DELETE FROM model_segments WHERE session_id = ?1",
            params![&id_str],
        )?;
        tx.execute(
            "DELETE FROM turn_metrics WHERE session_id = ?1",
            params![&id_str],
        )?;
        tx.execute(
            "DELETE FROM approvals WHERE session_id = ?1",
            params![&id_str],
        )?;
        tx.execute(
            "DELETE FROM offloaded_content WHERE session_id = ?1",
            params![&id_str],
        )?;
        tx.execute(
            "DELETE FROM context_snapshots WHERE session_id = ?1",
            params![&id_str],
        )?;
        tx.execute(
            "DELETE FROM conversation_events WHERE session_id = ?1",
            params![&id_str],
        )?;
        let diagnostics_table_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_diagnostics')",
            [],
            |row| row.get(0),
        )?;
        if diagnostics_table_exists {
            tx.execute(
                "DELETE FROM session_diagnostics WHERE session_id = ?1",
                params![&id_str],
            )?;
        }
        tx.execute("DELETE FROM sessions WHERE id = ?1", params![&id_str])?;
        tx.commit()?;
        Ok(())
    }

    /// Legacy alias kept for tests — performs a hard delete.
    pub fn delete_session(&self, id: Uuid) -> Result<()> {
        self.purge_session(id)
    }

    /// Load sessions filtered by project ID.
    /// If project_id is None, returns only unassigned sessions.
    /// Use load_sessions() to get all sessions regardless of project.
    pub fn load_sessions_by_project(&self, project_id: Option<Uuid>) -> Result<Vec<Session>> {
        let (sql, pid_str) = match project_id {
            Some(pid) => (
                format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions WHERE project_id = ?1 AND status != 'Archived' AND status != 'Deleted' ORDER BY created_at ASC"
                ),
                Some(pid.to_string()),
            ),
            None => (
                format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions WHERE project_id IS NULL AND status != 'Archived' AND status != 'Deleted' ORDER BY created_at ASC"
                ),
                None,
            ),
        };

        let mut stmt = self.conn.prepare(&sql)?;

        let sessions = match pid_str {
            Some(pid) => stmt
                .query_map(params![pid], map_session_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            None => stmt
                .query_map([], map_session_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };

        let mut sessions: Vec<Session> = sessions
            .into_iter()
            .map(|row| row.into_session())
            .collect::<Result<Vec<_>>>()?;

        // Hydrate tags from session_tags (P1.5).
        self.hydrate_tags(&mut sessions)?;

        Ok(sessions)
    }

    /// Populate `Session.tags` and `Session.tag` from `session_tags` rows.
    ///
    /// Runs a single batched `SELECT session_id, tag FROM session_tags WHERE
    /// session_id IN (...)` query. Each session's tags are sorted; the first
    /// (alphabetically) becomes `Session.tag`. Sessions with no rows keep the
    /// `tags: vec![]` / `tag: ""` defaults from `into_session()`.
    fn hydrate_tags(&self, sessions: &mut Vec<Session>) -> Result<()> {
        if sessions.is_empty() {
            return Ok(());
        }

        // Build `?,?,?` placeholder list.
        let ids: Vec<String> = sessions.iter().map(|s| s.id.to_string()).collect();
        let placeholders: String = ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");

        let sql = format!(
            "SELECT session_id, tag FROM session_tags WHERE session_id IN ({}) ORDER BY session_id, tag ASC",
            placeholders
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

        let mut map: HashMap<Uuid, Vec<String>> = HashMap::new();
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            let session_id_str: String = row.get(0)?;
            let tag: String = row.get(1)?;
            Ok((session_id_str, tag))
        })?;

        for row in rows {
            let (session_id_str, tag) = row?;
            if let Ok(session_id) = session_id_str.parse::<Uuid>() {
                map.entry(session_id).or_default().push(tag);
            }
        }

        for session in sessions.iter_mut() {
            if let Some(mut tags) = map.remove(&session.id) {
                tags.sort();
                session.tag = tags.first().cloned().unwrap_or_default();
                session.tags = tags;
            }
        }

        Ok(())
    }

    /// Update a session's project_id.
    pub fn update_session_project(&self, session_id: Uuid, project_id: Option<Uuid>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET project_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                project_id.map(|id| id.to_string()),
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Stamp a fired scheduled job ID onto an existing session row.
    /// Used by Resume-mode scheduled jobs that re-invoke the origin session
    /// rather than spawning a new one.
    pub fn update_session_scheduled_job_id(
        &self,
        session_id: Uuid,
        job_id: Option<Uuid>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET scheduled_job_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                job_id.map(|id| id.to_string()),
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Update a session's title.
    ///
    /// This is the deliberate-write path: rename and any other caller that
    /// means to set the session's identity unconditionally.
    /// Generated-title enrichment must use [`Self::fill_session_title_if_absent`]
    /// instead so it cannot overwrite a title the operator chose.
    pub fn update_session_title(&self, session_id: Uuid, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                title,
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Fill a session's title only while it is still absent.
    ///
    /// This is the generated-enrichment path. It is conditional because an
    /// explicit title is the session's identity: asynchronous enrichment may
    /// supply a title that is missing, but must never replace one that exists.
    /// The single guarded statement is the arbiter, so an explicit title
    /// written at any point before this call wins regardless of how long
    /// generation took.
    ///
    /// Returns `true` when this call supplied the title and `false` when the
    /// session already had one. A missing session is an error rather than a
    /// silent no-op so lost enrichment is visible.
    pub fn fill_session_title_if_absent(&self, session_id: Uuid, title: &str) -> Result<bool> {
        let updated = self.conn.execute(
            "UPDATE sessions SET title = ?1, updated_at = ?2
             WHERE id = ?3 AND (title IS NULL OR trim(title) = '')",
            params![
                title,
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        if updated > 0 {
            return Ok(true);
        }
        let exists: Option<()> = self
            .conn
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![session_id.to_string()],
                |_| Ok(()),
            )
            .optional()?;
        if exists.is_none() {
            return Err(DaemonError::SessionNotFound(session_id));
        }
        Ok(false)
    }

    /// Update a session's description.
    pub fn update_session_description(&self, session_id: Uuid, description: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET description = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                description,
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Update a session's active task context.
    pub fn update_session_active_task(
        &self,
        session_id: Uuid,
        active_task: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET active_task = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                active_task,
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Load archived sessions from the preceding seven days, optionally filtered by project_id.
    /// Returns Vec<Session> sorted by updated_at DESC (most recently archived first).
    pub fn load_archived_sessions(&self, project_id: Option<Uuid>) -> Result<Vec<Session>> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(ARCHIVED_SESSION_LOOKBACK_DAYS))
            .to_rfc3339();
        let (sql, pid_str) = match project_id {
            Some(pid) => (
                format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions WHERE status = 'Archived' AND project_id = ?1 AND updated_at >= ?2 ORDER BY updated_at DESC"
                ),
                Some(pid.to_string()),
            ),
            None => (
                format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions WHERE status = 'Archived' AND updated_at >= ?1 ORDER BY updated_at DESC"
                ),
                None,
            ),
        };

        let mut stmt = self.conn.prepare(&sql)?;

        let sessions = match pid_str {
            Some(pid) => stmt
                .query_map(params![pid, cutoff], map_session_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            None => stmt
                .query_map(params![cutoff], map_session_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };

        sessions.into_iter().map(|row| row.into_session()).collect()
    }

    /// Load deleted (soft-deleted) sessions, optionally filtered by project_id.
    /// Returns Vec<Session> sorted by updated_at DESC (most recently deleted first).
    pub fn load_deleted_sessions(&self, project_id: Option<Uuid>) -> Result<Vec<Session>> {
        let (sql, pid_str) = match project_id {
            Some(pid) => (
                format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions WHERE status = 'Deleted' AND project_id = ?1 ORDER BY updated_at DESC"
                ),
                Some(pid.to_string()),
            ),
            None => (
                format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions WHERE status = 'Deleted' ORDER BY updated_at DESC"
                ),
                None,
            ),
        };

        let mut stmt = self.conn.prepare(&sql)?;

        let sessions = match pid_str {
            Some(pid) => stmt
                .query_map(params![pid], map_session_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            None => stmt
                .query_map([], map_session_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };

        sessions.into_iter().map(|row| row.into_session()).collect()
    }

    /// Undelete a session: set status back to Completed, return the updated session.
    pub fn undelete_session(&self, id: Uuid) -> Result<Option<Session>> {
        self.restore_historical_session(id, "Deleted")
    }

    /// Unarchive a session: set status back to Completed, return the updated session.
    pub fn unarchive_session(&self, id: Uuid) -> Result<Option<Session>> {
        self.restore_historical_session(id, "Archived")
    }

    fn restore_historical_session(
        &self,
        id: Uuid,
        expected_status: &str,
    ) -> Result<Option<Session>> {
        let tx = self.conn.unchecked_transaction()?;
        let settlement_blocks = historical_session_restore_blocked_on(&tx, id)?;
        if settlement_blocks {
            return Err(crate::error::DaemonError::InvalidParam(
                "source-worktree settlement history forbids restoring this Session".into(),
            ));
        }
        let changed = tx.execute(
            "UPDATE sessions SET status='Completed',pending_archive=0,updated_at=?1
             WHERE id=?2 AND status=?3",
            params![
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                id.to_string(),
                expected_status,
            ],
        )?;
        if changed != 1 {
            return Ok(None);
        }
        tx.commit()?;
        self.get_session(id)
    }

    /// Update a session's label assignment (column `group_id` preserved).
    pub fn update_session_label(&self, session_id: Uuid, group_id: Option<Uuid>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET group_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                group_id.map(|id| id.to_string()),
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Set or clear the lead pointer on a container row. Atomic against
    /// concurrent assignments via the WHERE clause when assigning.
    pub fn set_lead_session(&self, container_id: Uuid, lead: Option<Uuid>) -> Result<()> {
        self.reject_nonterminal_agent_successor_epic_lead_mutation(container_id)?;
        self.conn.execute(
            "UPDATE sessions SET lead_session_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                lead.map(|id| id.to_string()),
                chrono::Utc::now().to_rfc3339(),
                container_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Operator lead reassignment and mail transfer share one commit boundary.
    pub(crate) fn set_epic_lead_and_readdress(
        &self,
        epic: Uuid,
        new_lead: Option<Uuid>,
    ) -> Result<Vec<Uuid>> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let old_lead: Option<String> = self.conn.query_row(
            "SELECT lead_session_id FROM sessions WHERE id=?1 AND session_kind='Epic'",
            [epic.to_string()],
            |row| row.get(0),
        )?;
        self.set_lead_session(epic, new_lead)?;
        let jobs = match (old_lead, new_lead) {
            (Some(old), Some(new)) if old != new.to_string() => {
                let old = Uuid::parse_str(&old).map_err(|_| {
                    crate::error::DaemonError::InvalidParam("invalid stored lead".into())
                })?;
                self.readdress_open_manager_requests(epic, old, new)?
            }
            _ => Vec::new(),
        };
        tx.commit()?;
        Ok(jobs)
    }

    /// Atomic auto-promote: only succeeds if no lead is currently set.
    /// Returns `true` if this call became the lead, `false` if a concurrent
    /// caller had already won.
    pub fn try_promote_lead_if_unset(&self, container_id: Uuid, candidate: Uuid) -> Result<bool> {
        self.reject_nonterminal_agent_successor_epic_lead_mutation(container_id)?;
        let n = self.conn.execute(
            "UPDATE sessions SET lead_session_id = ?1, updated_at = ?2 \
             WHERE id = ?3 AND lead_session_id IS NULL",
            params![
                candidate.to_string(),
                chrono::Utc::now().to_rfc3339(),
                container_id.to_string()
            ],
        )?;
        Ok(n == 1)
    }

    /// Pre-delete hook: NULL out any container row whose lead_session_id
    /// points at the session being deleted.
    pub fn clear_lead_session_if_matches(&self, target: Uuid) -> Result<()> {
        self.reject_nonterminal_agent_successor_lead_clear(target)?;
        self.conn.execute(
            "UPDATE sessions SET lead_session_id = NULL, updated_at = ?1 WHERE lead_session_id = ?2",
            params![chrono::Utc::now().to_rfc3339(), target.to_string()],
        )?;
        Ok(())
    }

    /// Rotation hook: find every container row where lead_session_id == old.
    pub fn find_epics_by_lead(&self, lead: Uuid) -> Result<Vec<Uuid>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM sessions WHERE lead_session_id = ?1")?;
        let rows = stmt
            .query_map([lead.to_string()], |row| {
                let s: String = row.get(0)?;
                Ok(s)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|s| Uuid::parse_str(&s).map_err(|e| DaemonError::Store(e.to_string())))
            .collect()
    }

    /// Refuse a context rotation before provider/controller effects when the
    /// predecessor already has a live or committed master-successor
    /// reservation, or when any affected Epic is fenced by a nonterminal one.
    ///
    /// The two checks are not redundant. The Epic check keys off
    /// `lead_session_id`, which a *committed* reservation has already moved to
    /// its candidate, so `affected` is empty and the loop passes vacuously —
    /// which is how a rotation minutes behind a committed baton handoff used to
    /// slip through and give the predecessor a second `continued_from`
    /// successor. The predecessor check closes that by asking about the
    /// predecessor directly, in every reservation state that owns a
    /// continuation.
    ///
    /// The successor-reservation kernel is authoritative for Epic lead lineage,
    /// so context rotation is the mechanism that defers. Both of this
    /// function's call sites refuse before any provider effect, and both leave
    /// the predecessor `Completed` and restorable rather than archived.
    ///
    /// The final transfer repeats the Epic check transactionally to close
    /// races; the predecessor check is repeated inside the transaction that
    /// inserts the rotation successor row, so the branch cannot be raced.
    pub(crate) fn preflight_rotation_lead_transfer(&self, old_lead: Uuid) -> Result<Vec<Uuid>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        reject_agent_successor_predecessor_continuation_on(&tx, old_lead)?;
        let affected = find_epics_by_lead_on(&tx, old_lead)?;
        for epic_id in &affected {
            reject_nonterminal_agent_successor_epic_lead_mutation_on(&tx, *epic_id)?;
        }
        tx.commit()?;
        Ok(affected)
    }

    /// RPC-1 C1: publish a rotation successor in ONE `IMMEDIATE` transaction.
    ///
    /// The commit moves every Epic lead pointer from `predecessor` to
    /// `successor` (the `sessions_epic_lead_generation_change` trigger bumps
    /// each affected Epic's generation inside the same commit) and inserts
    /// the terminal `completed{successor_id,..}` rotation event. It refuses,
    /// changing nothing, when a nonterminal master-successor reservation
    /// locks an affected Epic, when `successor` is not a `continued_from`
    /// row of `predecessor`, or when `(predecessor, rotation_id)` already has
    /// a terminal event. The commit is the single publication point: the
    /// published tip (`find_published_rotation_successor`) and the lead
    /// generation can never be observed torn.
    ///
    /// # Errors
    /// Precondition (K2 finding a): the caller holds the spawn guards of
    /// both `predecessor` and `successor` in the global lock order, proven by
    /// the `RotationPublicationGuards` witness. A continuation holds its
    /// target's guard from its fence check until its provider is installed,
    /// so it either observes this commit or completes strictly before it.
    ///
    /// # Errors
    /// `PolicyDenied` for a lead lock or an already-settled rotation,
    /// `Store` for a missing successor row or lost lead witness.
    pub(crate) fn publish_rotation_successor(
        &self,
        guards: &crate::session::RotationPublicationGuards,
        rotation_id: &str,
        metadata: &str,
    ) -> Result<Vec<Uuid>> {
        let predecessor = guards.predecessor();
        let successor = guards.successor();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let settled: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1 AND rotation_id=?2
               AND (event_type IN ('completed','suppressed_final_handoff')
                    OR event_type LIKE 'refused:%'))",
            params![predecessor.to_string(), rotation_id],
            |row| row.get(0),
        )?;
        if settled {
            return Err(DaemonError::PolicyDenied(format!(
                "rotation_already_settled:{predecessor}:{rotation_id}"
            )));
        }
        let successor_row: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND continued_from=?2)",
            params![successor.to_string(), predecessor.to_string()],
            |row| row.get(0),
        )?;
        if !successor_row {
            return Err(DaemonError::Store(format!(
                "rotation_successor_row_missing:{predecessor}:{successor}"
            )));
        }
        let affected = find_epics_by_lead_on(&tx, predecessor)?;
        for epic_id in &affected {
            reject_nonterminal_agent_successor_epic_lead_mutation_on(&tx, *epic_id)?;
        }
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        for epic_id in &affected {
            let changed = tx.execute(
                "UPDATE sessions SET lead_session_id=?1,updated_at=?2
                 WHERE id=?3 AND lead_session_id=?4",
                params![
                    successor.to_string(),
                    now,
                    epic_id.to_string(),
                    predecessor.to_string(),
                ],
            )?;
            if changed != 1 {
                return Err(DaemonError::Store(format!(
                    "rotation Epic lead transfer lost durable witness:{epic_id}:{predecessor}"
                )));
            }
        }
        tx.execute(
            "INSERT INTO rotation_events (session_id, rotation_id, phase, event_type, metadata, created_at)
             VALUES (?1, ?2, 'completed', 'completed', ?3, ?4)",
            params![predecessor.to_string(), rotation_id, metadata, now],
        )?;
        tx.commit()?;
        Ok(affected)
    }

    /// RPC-1 C2: the published rotation tip of `predecessor`.
    ///
    /// Returns `S` iff `S.continued_from = predecessor` and `predecessor` has
    /// a `completed` rotation event whose `metadata.successor_id = S`. A
    /// reserved, launched-but-unpublished, or refused successor is never the
    /// tip. Legacy fallback, only when `predecessor` has no
    /// successor_id-bearing `completed` event: the newest
    /// `continued_from = predecessor` row that is not `Failed` and is a
    /// legacy row. Legacy is decided by durable provenance, not status
    /// (K2 finding b): every rotation reservation since RPC-1 writes a
    /// `successor_reserved{successor_id}` marker in the same transaction as
    /// the successor row, so a marked row is current and is the tip only
    /// once published. An `AgentReserveSuccessor` candidate is excluded until
    /// its reservation commits (RPC-1 C6).
    ///
    /// # Errors
    /// Fails on `SQLite` errors or a malformed UUID.
    pub fn find_published_rotation_successor(&self, predecessor: Uuid) -> Result<Option<Uuid>> {
        let published: Vec<String> = self
            .conn
            .prepare(
                "SELECT json_extract(metadata,'$.successor_id') FROM rotation_events
                 WHERE session_id=?1 AND event_type='completed'
                   AND json_valid(metadata)
                   AND json_extract(metadata,'$.successor_id') IS NOT NULL
                 ORDER BY id DESC",
            )?
            .query_map([predecessor.to_string()], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?;
        let candidate = if published.is_empty() {
            self.conn
                .query_row(
                    "SELECT s.id FROM sessions s WHERE s.continued_from=?1 AND s.status<>'Failed'
                       AND NOT EXISTS(SELECT 1 FROM rotation_events e
                                      WHERE e.session_id=?1 AND e.event_type='successor_reserved'
                                        AND json_valid(e.metadata)
                                        AND json_extract(e.metadata,'$.successor_id')=s.id)
                       AND NOT EXISTS(SELECT 1 FROM agent_successor_reservations r
                                      WHERE r.candidate_session_id=s.id AND r.state<>'committed')
                     ORDER BY s.created_at DESC LIMIT 1",
                    [predecessor.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
        } else {
            let mut found = None;
            for successor in published {
                let row: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND continued_from=?2)",
                    params![successor, predecessor.to_string()],
                    |row| row.get(0),
                )?;
                if row {
                    found = Some(successor);
                    break;
                }
            }
            found
        };
        candidate
            .map(|id| Uuid::parse_str(&id).map_err(|error| DaemonError::Store(error.to_string())))
            .transpose()
    }

    /// Reparent a session in the hierarchy (Group/Epic organizational tree).
    /// `new_parent = None` means "move to top-level". Callers should validate
    /// containment legality and cycle-freeness before calling this; this fn
    /// performs the raw write only.
    pub fn update_session_parent(&self, session_id: Uuid, new_parent: Option<Uuid>) -> Result<()> {
        self.reject_nonterminal_agent_successor_candidate_topology_mutation(session_id)?;
        self.conn.execute(
            "UPDATE sessions SET parent_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                new_parent.map(|id| id.to_string()),
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// List direct children of a hierarchy parent. `None` returns top-level
    /// sessions (rows with `parent_id IS NULL`). Read against the
    /// `idx_sessions_parent_id` index, so cost scales with result size.
    pub fn list_children(&self, parent: Option<Uuid>) -> Result<Vec<Session>> {
        let sql = format!(
            "SELECT {} FROM sessions WHERE parent_id IS ?1 ORDER BY created_at ASC",
            SESSION_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![parent.map(|id| id.to_string())], map_session_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(|r| r.into_session()).collect()
    }

    /// List every descendant of a hierarchy root. The hierarchy is normally
    /// shallow, but walking it here keeps lifecycle operations correct if new
    /// container levels are introduced later. A repeated node is durable
    /// hierarchy corruption, not an excuse to loop forever.
    pub fn list_descendants(&self, root_id: Uuid) -> Result<Vec<Session>> {
        let mut descendants = Vec::new();
        let mut pending = vec![root_id];
        let mut seen = std::collections::HashSet::from([root_id]);

        while let Some(parent_id) = pending.pop() {
            for child in self.list_children(Some(parent_id))? {
                if !seen.insert(child.id) {
                    return Err(DaemonError::Store(format!(
                        "hierarchy cycle encountered while enumerating descendants of {root_id}"
                    )));
                }
                pending.push(child.id);
                descendants.push(child);
            }
        }

        Ok(descendants)
    }

    /// Snapshot every session_id -> parent_id mapping in a single pass.
    /// Used by the hierarchy cycle detector so the validator can run as a
    /// pure-fn closure over the snapshot rather than issuing one DB call
    /// per ancestor walk.
    pub fn load_parent_index(&self) -> Result<std::collections::HashMap<Uuid, Option<Uuid>>> {
        let mut stmt = self.conn.prepare("SELECT id, parent_id FROM sessions")?;
        let mut out = std::collections::HashMap::new();
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let parent: Option<String> = row.get(1)?;
            Ok((id, parent))
        })?;
        for row in rows {
            let (id_str, parent_str) = row?;
            let id = Uuid::parse_str(&id_str)
                .map_err(|e| DaemonError::Store(format!("invalid session id: {}", e)))?;
            let parent = match parent_str {
                Some(p) => Some(
                    Uuid::parse_str(&p)
                        .map_err(|e| DaemonError::Store(format!("invalid parent id: {}", e)))?,
                ),
                None => None,
            };
            out.insert(id, parent);
        }
        Ok(out)
    }

    /// Cheap existence check: does any row have `parent_id = ?`.
    pub fn has_children(&self, parent_id: Uuid) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE parent_id = ?1",
            params![parent_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Update the pending_archive flag for a session.
    pub fn update_pending_archive(&self, id: Uuid, pending_archive: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET pending_archive = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                pending_archive as i64,
                chrono::Utc::now().to_rfc3339(),
                id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Toggle testing-needed state. Returns the new testing_needed_at value.
    pub fn toggle_session_testing_needed(&self, id: Uuid) -> Result<Option<String>> {
        let id_str = id.to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let rows = self.conn.execute(
            "UPDATE sessions SET testing_needed_at = CASE WHEN testing_needed_at IS NULL THEN ?1 ELSE NULL END WHERE id = ?2",
            params![&now, &id_str],
        )?;

        if rows == 0 {
            return Err(DaemonError::Store("session not found".to_string()));
        }

        let new_val: Option<String> = self.conn.query_row(
            "SELECT testing_needed_at FROM sessions WHERE id = ?1",
            params![&id_str],
            |row| row.get(0),
        )?;

        Ok(new_val)
    }

    /// Toggle rotation-disabled state. Returns the new rotation_disabled_at value.
    pub fn toggle_session_rotation_disabled(&self, id: Uuid) -> Result<Option<String>> {
        let id_str = id.to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let rows = self.conn.execute(
            "UPDATE sessions SET rotation_disabled_at = CASE WHEN rotation_disabled_at IS NULL THEN ?1 ELSE NULL END WHERE id = ?2",
            params![&now, &id_str],
        )?;

        if rows == 0 {
            return Err(DaemonError::Store("session not found".to_string()));
        }

        let new_val: Option<String> = self.conn.query_row(
            "SELECT rotation_disabled_at FROM sessions WHERE id = ?1",
            params![&id_str],
            |row| row.get(0),
        )?;

        Ok(new_val)
    }

    /// Persist an explicit rotation-disabled value without toggle semantics.
    /// Establishment paths use this idempotent setter when the in-memory
    /// session already carries an inherited timestamp.
    pub fn set_session_rotation_disabled_at(
        &self,
        id: Uuid,
        rotation_disabled_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<()> {
        let rows = self.conn.execute(
            "UPDATE sessions SET rotation_disabled_at = ?1 WHERE id = ?2",
            params![
                rotation_disabled_at
                    .map(|value| { value.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true) }),
                id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(DaemonError::Store("session not found".to_string()));
        }
        Ok(())
    }

    /// Update retry tracking fields for a session.
    pub fn update_retry_state(
        &self,
        session_id: Uuid,
        retry_attempt: Option<u8>,
        max_retries: Option<u8>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET retry_attempt = ?1, max_retries = ?2, updated_at = ?3 WHERE id = ?4",
            params![
                retry_attempt.map(|v| v as i64),
                max_retries.map(|v| v as i64),
                chrono::Utc::now().to_rfc3339(),
                session_id.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Pin a session unless it is already pinned. Returns the effective
    /// `pinned_at`, which is the pre-existing value when one was already set.
    ///
    /// Deliberately NOT [`Self::toggle_session_pin`]: an automatic pin that
    /// toggles would UNPIN a session the operator had already pinned by hand,
    /// and would unpin a lead that takes the baton twice. Preserving the
    /// original `pinned_at` also preserves pin ordering, which is what makes
    /// the pinned set readable as a succession line.
    pub fn pin_session_if_unpinned(&self, id: Uuid) -> Result<Option<String>> {
        let id_str = id.to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let rows = self.conn.execute(
            "UPDATE sessions SET pinned_at = ?1, updated_at = ?2 \
             WHERE id = ?3 AND pinned_at IS NULL",
            params![&now, &now, &id_str],
        )?;
        if rows == 0 {
            // Either already pinned or no such row; the read-back distinguishes.
            let existing: Option<Option<String>> = self
                .conn
                .query_row(
                    "SELECT pinned_at FROM sessions WHERE id = ?1",
                    params![&id_str],
                    |row| row.get(0),
                )
                .optional()?;
            return match existing {
                Some(pinned_at) => Ok(pinned_at),
                None => Err(DaemonError::Store("session not found".to_string())),
            };
        }

        Ok(Some(now))
    }

    /// Toggle pin state. Returns the new pinned_at value.
    pub fn toggle_session_pin(&self, id: Uuid) -> Result<Option<String>> {
        let id_str = id.to_string();
        let now = chrono::Utc::now().to_rfc3339();

        // Atomic toggle: set to now if NULL, set to NULL if non-NULL
        let rows = self.conn.execute(
            "UPDATE sessions SET pinned_at = CASE WHEN pinned_at IS NULL THEN ?1 ELSE NULL END WHERE id = ?2",
            params![&now, &id_str],
        )?;

        if rows == 0 {
            return Err(DaemonError::Store("session not found".to_string()));
        }

        // Read back the new value
        let new_val: Option<String> = self.conn.query_row(
            "SELECT pinned_at FROM sessions WHERE id = ?1",
            params![&id_str],
            |row| row.get(0),
        )?;

        Ok(new_val)
    }

    /// Load session IDs that are in active statuses (Running, Starting, WaitingApproval).
    /// Used by the reconciliation loop for SQLite consistency checks.
    pub fn load_active_session_ids(&self) -> Result<Vec<Uuid>> {
        self.load_active_session_ids_bounded(STARTUP_PROVIDER_CANDIDATE_MAX)
    }

    /// Authorize only the exact process stamps observed by the bounded startup
    /// `/proc` inventory. Session ownership is durable in any Session status;
    /// the two launch journals additionally cover the pre-Session-insert
    /// window. Model invocation rows are immutable durable execution identity
    /// and remain authoritative in every invocation state.
    ///
    /// One JSON-backed statement authorizes the complete bounded set from one
    /// SQLite snapshot. A deadline-scoped busy timeout plus interrupt watchdog
    /// prevents the Store's ordinary ten-second busy policy (or a hostile query
    /// plan) from escaping the process fence's remaining wall-clock budget.
    pub(crate) fn authorize_startup_process_ids(
        &self,
        observed_session_ids: &[Uuid],
        observed_invocation_ids: &[Uuid],
        deadline: Instant,
    ) -> Result<(HashSet<Uuid>, HashSet<Uuid>)> {
        if observed_session_ids.len() > STARTUP_PROVIDER_CANDIDATE_MAX
            || observed_invocation_ids.len() > STARTUP_PROVIDER_CANDIDATE_MAX
        {
            return Err(DaemonError::StartupProviderInventory(format!(
                "startup process ownership candidate bound exceeded ({STARTUP_PROVIDER_CANDIDATE_MAX})"
            )));
        }

        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                DaemonError::StartupProviderInventory(
                    "startup process ownership lookup deadline expired".into(),
                )
            })?;
        if remaining.is_zero() {
            return Err(DaemonError::StartupProviderInventory(
                "startup process ownership lookup deadline expired".into(),
            ));
        }

        let session_json = serde_json::to_string(
            &observed_session_ids
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<_>>(),
        )
        .map_err(|error| {
            DaemonError::StartupProviderInventory(format!(
                "startup Session ownership input encoding failed: {error}"
            ))
        })?;
        let invocation_json = serde_json::to_string(
            &observed_invocation_ids
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<_>>(),
        )
        .map_err(|error| {
            DaemonError::StartupProviderInventory(format!(
                "startup model-invocation ownership input encoding failed: {error}"
            ))
        })?;

        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                DaemonError::StartupProviderInventory(
                    "startup process ownership lookup deadline expired".into(),
                )
            })?;
        if remaining.is_zero() {
            return Err(DaemonError::StartupProviderInventory(
                "startup process ownership lookup deadline expired".into(),
            ));
        }
        self.conn.busy_timeout(remaining).map_err(|error| {
            DaemonError::StartupProviderInventory(format!(
                "startup process ownership busy deadline setup failed: {error}"
            ))
        })?;
        let interrupt = self.conn.get_interrupt_handle();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let watchdog = match std::thread::Builder::new()
            .name("startup-ownership-sql-deadline".into())
            .spawn(move || {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero()
                    || matches!(
                        done_rx.recv_timeout(remaining),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    )
                {
                    interrupt.interrupt();
                }
            }) {
            Ok(watchdog) => watchdog,
            Err(error) => {
                let _ = self.conn.busy_timeout(Duration::from_secs(10));
                return Err(DaemonError::StartupProviderInventory(format!(
                    "startup process ownership deadline watchdog failed: {error}"
                )));
            }
        };

        let query_result = (|| -> Result<(HashSet<Uuid>, HashSet<Uuid>)> {
            let mut statement = self.conn.prepare(
                "WITH
                     observed_sessions(id) AS (
                         SELECT CAST(value AS TEXT) FROM json_each(?1)
                     ),
                     observed_invocations(id) AS (
                         SELECT CAST(value AS TEXT) FROM json_each(?2)
                     ),
                     owned(kind, id) AS (
                         SELECT 0, observed_sessions.id
                           FROM observed_sessions
                          WHERE EXISTS(SELECT 1 FROM sessions WHERE sessions.id=observed_sessions.id)
                             OR EXISTS(
                                 SELECT 1 FROM agent_spawn_requests
                                  WHERE child_session_id=observed_sessions.id AND state='launching'
                             )
                             OR EXISTS(
                                 SELECT 1 FROM agent_successor_reservations
                                  WHERE candidate_session_id=observed_sessions.id
                                    AND state IN ('launching','uncertain')
                             )
                         UNION ALL
                         SELECT 1, observed_invocations.id
                           FROM observed_invocations
                          WHERE EXISTS(
                              SELECT 1 FROM model_invocations
                               WHERE model_invocations.id=observed_invocations.id
                          )
                     )
                 SELECT kind, id FROM owned ORDER BY kind, id",
            )?;
            let rows = statement.query_map(params![session_json, invocation_json], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut owned_session_ids = HashSet::with_capacity(observed_session_ids.len());
            let mut owned_invocation_ids = HashSet::with_capacity(observed_invocation_ids.len());
            for row in rows {
                let (kind, raw_id) = row?;
                let id = Uuid::parse_str(&raw_id).map_err(|error| {
                    DaemonError::StartupProviderInventory(format!(
                        "startup process ownership query returned invalid UUID: {error}"
                    ))
                })?;
                match kind {
                    0 => {
                        owned_session_ids.insert(id);
                    }
                    1 => {
                        owned_invocation_ids.insert(id);
                    }
                    _ => {
                        return Err(DaemonError::StartupProviderInventory(
                            "startup process ownership query returned invalid identity kind"
                                .into(),
                        ));
                    }
                }
            }
            Ok((owned_session_ids, owned_invocation_ids))
        })()
        .map_err(|error| match error {
            error @ DaemonError::StartupProviderInventory(_) => error,
            error => DaemonError::StartupProviderInventory(format!(
                "startup process ownership snapshot failed: {error}"
            )),
        });

        let _ = done_tx.send(());
        let watchdog_result = watchdog.join();
        let restore_result = self.conn.busy_timeout(Duration::from_secs(10));
        if watchdog_result.is_err() {
            return Err(DaemonError::StartupProviderInventory(
                "startup process ownership deadline watchdog panicked".into(),
            ));
        }
        restore_result.map_err(|error| {
            DaemonError::StartupProviderInventory(format!(
                "startup process ownership busy deadline restore failed: {error}"
            ))
        })?;
        if Instant::now() >= deadline {
            return Err(DaemonError::StartupProviderInventory(
                "startup process ownership lookup deadline expired".into(),
            ));
        }
        query_result
    }

    fn load_active_session_ids_bounded(&self, max_candidates: usize) -> Result<Vec<Uuid>> {
        let query_limit = max_candidates
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or_else(|| {
                DaemonError::StartupProviderInventory(
                    "active-like Session candidate query limit overflowed".into(),
                )
            })?;
        let mut stmt = self.conn.prepare(
            "SELECT id FROM sessions
              WHERE status IN ('Running', 'Starting', 'WaitingApproval')
              ORDER BY id
              LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![query_limit], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })?;
        let mut ids = Vec::new();
        for raw in rows {
            if ids.len() >= max_candidates {
                return Err(DaemonError::StartupProviderInventory(format!(
                    "active-like Session candidate bound exceeded ({max_candidates})"
                )));
            }
            let raw = raw?;
            let id = Uuid::parse_str(&raw).map_err(|error| {
                DaemonError::Store(format!(
                    "invalid active-like session id during process inventory: {error}"
                ))
            })?;
            ids.push(id);
        }
        Ok(ids)
    }

    // === Issue tracker dispatch records ===

    /// Insert a new dispatch record for an issue-driven session.
    pub fn insert_issue_dispatch(
        &self,
        record: &crate::issue_tracker::types::DispatchRecord,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO issue_tracker_dispatches (issue_id, issue_identifier, tracker, session_id,
             dispatched_at, last_reconciled_at, terminal_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.issue_id,
                record.issue_identifier,
                record.tracker,
                record.session_id.to_string(),
                record.dispatched_at.to_rfc3339(),
                record.last_reconciled_at.map(|dt| dt.to_rfc3339()),
                record.terminal_state,
            ],
        )?;
        Ok(())
    }

    /// Update the last_reconciled_at timestamp for a dispatch record.
    pub fn update_issue_dispatch_reconciled(
        &self,
        issue_id: &str,
        reconciled_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE issue_tracker_dispatches SET last_reconciled_at = ?1 WHERE issue_id = ?2",
            params![reconciled_at.to_rfc3339(), issue_id],
        )?;
        Ok(())
    }

    /// Mark a dispatch record as terminal (session completed/failed).
    pub fn mark_issue_dispatch_terminal(&self, issue_id: &str, terminal_state: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE issue_tracker_dispatches SET terminal_state = ?1 WHERE issue_id = ?2",
            params![terminal_state, issue_id],
        )?;
        Ok(())
    }

    /// Project-binding gate for local tracker dispatches. The dispatch table
    /// deliberately carries no mutable project column: both the launched
    /// Session and the Issue must resolve to the construction-bound project.
    pub fn local_issue_dispatch_matches_project(
        &self,
        issue_id: &str,
        session_id: Uuid,
        project_id: Uuid,
    ) -> Result<bool> {
        let issue_id = Uuid::parse_str(issue_id).map_err(|error| {
            DaemonError::Store(format!("Invalid local issue UUID {issue_id}: {error}"))
        })?;
        self.conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1
                    FROM sessions s
                    JOIN issues i ON i.id = ?1 AND i.project_id = s.project_id
                    WHERE s.id = ?2 AND s.project_id = ?3
                 )",
                params![
                    issue_id.to_string(),
                    session_id.to_string(),
                    project_id.to_string(),
                ],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Load all active (non-terminal) dispatch records for state restoration.
    pub fn load_active_dispatches(
        &self,
    ) -> Result<Vec<crate::issue_tracker::types::DispatchRecord>> {
        use super::row_mappers::parse_timestamp;
        let mut stmt = self.conn.prepare(
            "SELECT issue_id, issue_identifier, tracker, session_id, dispatched_at,
                    last_reconciled_at, terminal_state
             FROM issue_tracker_dispatches
             WHERE terminal_state IS NULL",
        )?;
        let records = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .filter_map(
                |(
                    issue_id,
                    identifier,
                    tracker,
                    session_id_str,
                    dispatched_str,
                    reconciled_str,
                    terminal,
                )| {
                    let session_id = Uuid::parse_str(&session_id_str).ok()?;
                    let dispatched_at = parse_timestamp(&dispatched_str).ok()?;
                    let last_reconciled_at = reconciled_str
                        .as_deref()
                        .map(parse_timestamp)
                        .transpose()
                        .ok()?;
                    Some(crate::issue_tracker::types::DispatchRecord {
                        issue_id,
                        issue_identifier: identifier,
                        tracker,
                        session_id,
                        dispatched_at,
                        last_reconciled_at,
                        terminal_state: terminal,
                    })
                },
            )
            .collect();
        Ok(records)
    }

    /// Restore only local dispatches whose persisted Session and Issue still
    /// agree with the configured project. This uses the V77 active-dispatch
    /// index followed by exact primary-key joins; malformed or cross-project
    /// legacy rows stay durable but are never adopted into in-memory state.
    pub fn load_active_dispatches_for_tracker_project(
        &self,
        tracker: &str,
        project_id: Uuid,
    ) -> Result<Vec<crate::issue_tracker::types::DispatchRecord>> {
        use super::row_mappers::parse_timestamp;
        let mut stmt = self.conn.prepare(
            "SELECT d.issue_id, d.issue_identifier, d.tracker, d.session_id,
                    d.dispatched_at, d.last_reconciled_at, d.terminal_state
             FROM issue_tracker_dispatches d
             JOIN sessions s ON s.id = d.session_id
             JOIN issues i ON i.id = d.issue_id AND i.project_id = s.project_id
             WHERE d.terminal_state IS NULL
               AND d.tracker = ?1
               AND s.project_id = ?2
               AND i.project_id = ?2
               AND i.archived_at IS NULL
               AND i.status IN ('Open','InProgress')
             ORDER BY d.issue_id",
        )?;
        let records = stmt
            .query_map(params![tracker, project_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?
            .filter_map(|row| row.ok())
            .filter_map(
                |(
                    issue_id,
                    issue_identifier,
                    tracker,
                    session_id,
                    dispatched_at,
                    last_reconciled_at,
                    terminal_state,
                )| {
                    Some(crate::issue_tracker::types::DispatchRecord {
                        issue_id,
                        issue_identifier,
                        tracker,
                        session_id: Uuid::parse_str(&session_id).ok()?,
                        dispatched_at: parse_timestamp(&dispatched_at).ok()?,
                        last_reconciled_at: last_reconciled_at
                            .as_deref()
                            .map(parse_timestamp)
                            .transpose()
                            .ok()?,
                        terminal_state,
                    })
                },
            )
            .collect();
        Ok(records)
    }

    /// Load a single dispatch record by issue ID.
    pub fn load_dispatch_by_issue_id(
        &self,
        issue_id: &str,
    ) -> Result<Option<crate::issue_tracker::types::DispatchRecord>> {
        use super::row_mappers::parse_timestamp;
        let mut stmt = self.conn.prepare(
            "SELECT issue_id, issue_identifier, tracker, session_id, dispatched_at,
                    last_reconciled_at, terminal_state
             FROM issue_tracker_dispatches
             WHERE issue_id = ?1",
        )?;
        let mut rows = stmt.query(params![issue_id])?;
        if let Some(row) = rows.next()? {
            let issue_id: String = row.get(0)?;
            let identifier: String = row.get(1)?;
            let tracker: String = row.get(2)?;
            let session_id_str: String = row.get(3)?;
            let dispatched_str: String = row.get(4)?;
            let reconciled_str: Option<String> = row.get(5)?;
            let terminal: Option<String> = row.get(6)?;

            let session_id = Uuid::parse_str(&session_id_str)
                .map_err(|e| DaemonError::Store(format!("Invalid session UUID: {}", e)))?;
            let dispatched_at = parse_timestamp(&dispatched_str).map_err(DaemonError::Store)?;
            let last_reconciled_at = reconciled_str
                .as_deref()
                .map(parse_timestamp)
                .transpose()
                .map_err(DaemonError::Store)?;

            Ok(Some(crate::issue_tracker::types::DispatchRecord {
                issue_id,
                issue_identifier: identifier,
                tracker,
                session_id,
                dispatched_at,
                last_reconciled_at,
                terminal_state: terminal,
            }))
        } else {
            Ok(None)
        }
    }
}

fn find_epics_by_lead_on(connection: &Connection, lead: Uuid) -> Result<Vec<Uuid>> {
    let mut statement =
        connection.prepare("SELECT id FROM sessions WHERE lead_session_id=?1 ORDER BY id")?;
    statement
        .query_map([lead.to_string()], |row| row.get::<_, String>(0))?
        .map(|row| {
            let id = row?;
            Uuid::parse_str(&id).map_err(|error| DaemonError::Store(error.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod active_session_inventory_tests {
    use super::*;
    use crate::store::tests::make_test_session;

    #[test]
    fn codegraph_registry_reads_only_verified_current_custody() {
        let store = Store::open_in_memory().unwrap();
        let session = make_test_session();
        store.insert_session(&session).unwrap();
        let custody_id = Uuid::new_v4();
        let root = format!("/tmp/codegraph-custody-{custody_id}");
        let repo = format!("/tmp/codegraph-repo-{custody_id}");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO sandbox_custody_roots
             (custody_id,allocation_id,canonical_repo_dir,sandbox_root,sandbox_branch,
              repository_identity,source_commit,state,owner_session_id,generation,
              event_sequence,validation_state,created_at,updated_at)
             VALUES (?1,?1,?2,?3,'rsi/test','repo','0000000000000000000000000000000000000000',
                     'live',?4,1,1,'unverified',?5,?5)",
                params![
                    custody_id.to_string(),
                    repo,
                    root,
                    session.id.to_string(),
                    now
                ],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET sandbox_custody_id=?1,sandbox_kind='GitWorktree',
             sandbox_root=?2,sandbox_branch='rsi/test',sandbox_cleanup_state='Live'
             WHERE id=?3",
                params![custody_id.to_string(), root, session.id.to_string()],
            )
            .unwrap();
        assert!(
            store
                .list_codegraph_sandbox_registrations()
                .unwrap()
                .is_empty()
        );
        store
            .conn
            .execute(
                "UPDATE sandbox_custody_roots
             SET validation_state='verified',validated_generation=1,validated_at=?2
             WHERE custody_id=?1",
                params![custody_id.to_string(), now],
            )
            .unwrap();
        assert_eq!(
            store.list_codegraph_sandbox_registrations().unwrap(),
            vec![(custody_id, PathBuf::from(repo), PathBuf::from(root))]
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET sandbox_cleanup_state='Failed' WHERE id=?1",
                [session.id.to_string()],
            )
            .unwrap();
        assert!(
            store
                .list_codegraph_sandbox_registrations()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn active_session_inventory_rejects_limit_plus_one_without_unbounded_collection() {
        let store = Store::open_in_memory().expect("open in-memory store");
        for _ in 0..3 {
            store
                .insert_session(&make_test_session())
                .expect("insert active-like Session");
        }

        let error = store
            .load_active_session_ids_bounded(2)
            .expect_err("third active-like row must exceed the bounded inventory");
        assert!(matches!(
            error,
            DaemonError::StartupProviderInventory(message)
                if message == "active-like Session candidate bound exceeded (2)"
        ));
    }

    #[test]
    fn startup_process_authorization_uses_one_snapshot_and_restores_busy_timeout() {
        let store = Store::open_in_memory().expect("open in-memory store");
        let mut session = make_test_session();
        session.status = SessionStatus::Completed;
        store
            .insert_session(&session)
            .expect("insert owned Session");
        let invocation_id = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                  trigger_source,policy_snapshot_json,usage_confidence,created_at)
                 VALUES (?1,'session.title','background','background','paid_capable',
                         'admitted','running','startup-ownership-test','{}','unavailable',?2)",
                params![invocation_id.to_string(), chrono::Utc::now().to_rfc3339()],
            )
            .expect("insert immutable invocation identity");

        let (sessions, invocations) = store
            .authorize_startup_process_ids(
                &[session.id, Uuid::new_v4()],
                &[invocation_id, Uuid::new_v4()],
                Instant::now() + Duration::from_secs(1),
            )
            .expect("authorize one bounded snapshot");
        assert_eq!(sessions, HashSet::from([session.id]));
        assert_eq!(invocations, HashSet::from([invocation_id]));
        let busy_timeout_ms: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("read restored busy timeout");
        assert_eq!(busy_timeout_ms, 10_000);
    }

    #[test]
    fn startup_process_authorization_refuses_an_expired_deadline_without_querying() {
        let store = Store::open_in_memory().expect("open in-memory store");
        let error = store
            .authorize_startup_process_ids(&[Uuid::new_v4()], &[], Instant::now())
            .expect_err("expired ownership lookup must fail closed");
        assert!(matches!(
            error,
            DaemonError::StartupProviderInventory(message)
                if message.contains("deadline expired")
        ));
    }

    #[test]
    fn startup_process_authorization_enforces_remaining_deadline_under_database_lock() {
        let directory = tempfile::tempdir().expect("create Store directory");
        let database = directory.path().join("ownership-deadline.db");
        let store = Store::open(&database).expect("open startup Store");
        let mut session = make_test_session();
        session.status = SessionStatus::Completed;
        store
            .insert_session(&session)
            .expect("insert owned Session");
        store
            .conn
            .execute_batch("PRAGMA journal_mode=DELETE;")
            .expect("use lock-blocking journal mode");
        let blocker = Connection::open(&database).expect("open lock holder");
        blocker
            .execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;")
            .expect("hold exclusive database lock");

        let started = Instant::now();
        let error = store
            .authorize_startup_process_ids(
                &[session.id],
                &[],
                Instant::now() + Duration::from_millis(100),
            )
            .expect_err("locked ownership snapshot must respect remaining deadline");
        let elapsed = started.elapsed();
        blocker
            .execute_batch("ROLLBACK;")
            .expect("release Store lock");
        drop(blocker);

        assert!(
            matches!(error, DaemonError::StartupProviderInventory(_)),
            "{error}"
        );
        assert!(elapsed < Duration::from_secs(1), "elapsed: {elapsed:?}");
        let persisted_status: String = store
            .conn
            .query_row(
                "SELECT status FROM sessions WHERE id=?1",
                [session.id.to_string()],
                |row| row.get(0),
            )
            .expect("read unchanged Session after failed authorization");
        assert_eq!(persisted_status, "Completed");
    }

    #[test]
    fn startup_process_authorization_sql_keeps_pre_row_and_immutable_owners() {
        let source = include_str!("sessions.rs");
        let start = source
            .find("pub(crate) fn authorize_startup_process_ids")
            .expect("startup authorization function");
        let end = source[start..]
            .find("fn load_active_session_ids_bounded")
            .map(|offset| start + offset)
            .expect("next Store function");
        let body = &source[start..end];

        assert!(body.contains("FROM json_each(?1)"));
        assert!(body.contains("FROM json_each(?2)"));
        assert!(body.contains("FROM sessions"));
        assert!(body.contains("FROM agent_spawn_requests"));
        assert!(body.contains("FROM agent_successor_reservations"));
        assert!(body.contains("FROM model_invocations"));
        assert!(!body.contains("WHERE id=?1"));
        assert!(!body.contains("INSERT INTO"));
        assert!(!body.contains("UPDATE "));
        assert!(!body.contains("DELETE FROM"));
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod rotation_publication_tests {
    use super::*;
    use crate::store::tests::make_test_session;
    use rsi_common::types::SessionKind;

    struct Lineage {
        epic: Uuid,
        predecessor: Uuid,
        successor: Uuid,
    }

    /// Epic led by `predecessor`, with an open rotation intent and a
    /// reserved `continued_from` successor row (the pre-publication state).
    fn reserved_rotation(store: &Store, rotation_id: &str) -> Lineage {
        let mut epic = make_test_session();
        epic.session_kind = SessionKind::Epic;
        let mut predecessor = make_test_session();
        predecessor.status = SessionStatus::Completed;
        predecessor.parent_id = Some(epic.id);
        let mut successor = make_test_session();
        successor.status = SessionStatus::Starting;
        successor.parent_id = Some(epic.id);
        successor.continued_from = Some(predecessor.id);
        successor.rotation_depth = 1;
        store.insert_session(&epic).expect("insert epic");
        store
            .insert_session(&predecessor)
            .expect("insert predecessor");
        store
            .set_lead_session(epic.id, Some(predecessor.id))
            .expect("lead");
        store
            .insert_rotation_event(
                predecessor.id,
                rotation_id,
                "writing_handoff",
                "entered",
                None,
            )
            .expect("intent");
        store.insert_session(&successor).expect("insert successor");
        // The durable reservation marker the rotation reservation writes in
        // the same transaction as the successor row.
        store
            .insert_rotation_event(
                predecessor.id,
                rotation_id,
                "reserved",
                "successor_reserved",
                Some(&serde_json::json!({ "successor_id": successor.id }).to_string()),
            )
            .expect("reservation marker");
        Lineage {
            epic: epic.id,
            predecessor: predecessor.id,
            successor: successor.id,
        }
    }

    /// One store-lock read of (published tip, lead, lead generation).
    fn observe(store: &Store, lineage: &Lineage) -> (Option<Uuid>, Option<Uuid>, i64) {
        let tip = store
            .find_published_rotation_successor(lineage.predecessor)
            .expect("published tip");
        let lead = store
            .get_session(lineage.epic)
            .expect("epic read")
            .expect("epic row")
            .lead_session_id;
        let generation: i64 = store
            .conn
            .query_row(
                "SELECT generation FROM epic_lead_generations WHERE epic_id=?1",
                [lineage.epic.to_string()],
                |row| row.get(0),
            )
            .expect("generation");
        (tip, lead, generation)
    }

    fn completed_event_count(store: &Store, predecessor: Uuid) -> i64 {
        store
            .conn
            .query_row(
                "SELECT count(*) FROM rotation_events WHERE session_id=?1 AND event_type='completed'",
                [predecessor.to_string()],
                |row| row.get(0),
            )
            .expect("event count")
    }

    async fn guards(lineage: &Lineage) -> crate::session::RotationPublicationGuards {
        crate::session::RotationPublicationGuards::acquire(lineage.predecessor, lineage.successor)
            .await
    }

    #[tokio::test]
    async fn publish_rotation_successor_moves_lead_generation_and_tip_in_one_commit() {
        let store = Store::open_in_memory().expect("store");
        let lineage = reserved_rotation(&store, "rot-1");
        let (tip, lead, generation) = observe(&store, &lineage);
        assert_eq!(
            tip, None,
            "a reserved, unpublished successor is not the tip"
        );
        assert_eq!(lead, Some(lineage.predecessor));

        // A failure after the lead UPDATE (the event insert) must roll the
        // lead and its generation back with it: one commit, no torn state.
        store
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_rotation_event BEFORE INSERT ON rotation_events
                 WHEN NEW.event_type='completed' BEGIN SELECT RAISE(ABORT,'injected'); END;",
            )
            .expect("fault trigger");
        assert!(
            store
                .publish_rotation_successor(
                    &guards(&lineage).await,
                    "rot-1",
                    r#"{"successor_id":"x"}"#,
                )
                .is_err()
        );
        assert_eq!(
            observe(&store, &lineage),
            (None, Some(lineage.predecessor), generation),
            "a failed publication leaves the predecessor tip, lead, and generation"
        );
        store
            .conn
            .execute_batch("DROP TRIGGER fail_rotation_event;")
            .expect("drop fault trigger");

        let metadata = serde_json::json!({ "successor_id": lineage.successor }).to_string();
        let affected = store
            .publish_rotation_successor(&guards(&lineage).await, "rot-1", &metadata)
            .expect("publish");
        assert_eq!(affected, vec![lineage.epic]);
        assert_eq!(
            observe(&store, &lineage),
            (
                Some(lineage.successor),
                Some(lineage.successor),
                generation + 1
            )
        );
        assert_eq!(completed_event_count(&store, lineage.predecessor), 1);

        // A replay of the settled rotation is refused and changes nothing.
        assert!(
            store
                .publish_rotation_successor(&guards(&lineage).await, "rot-1", &metadata)
                .is_err()
        );
        assert_eq!(completed_event_count(&store, lineage.predecessor), 1);
        assert_eq!(observe(&store, &lineage).2, generation + 1);
    }

    #[test]
    fn published_successor_lookup_ignores_failed_legacy_row() {
        let store = Store::open_in_memory().expect("store");
        let mut predecessor = make_test_session();
        predecessor.status = SessionStatus::Archived;
        store.insert_session(&predecessor).expect("predecessor");
        // Pre-RPC-1 rotation: `completed` without a successor_id.
        store
            .insert_rotation_event(predecessor.id, "legacy", "completed", "completed", None)
            .expect("legacy event");
        let mut live = make_test_session();
        live.status = SessionStatus::Completed;
        live.continued_from = Some(predecessor.id);
        live.created_at = chrono::Utc::now() - chrono::Duration::seconds(60);
        let mut failed = make_test_session();
        failed.status = SessionStatus::Failed;
        failed.continued_from = Some(predecessor.id);
        store.insert_session(&live).expect("live legacy successor");
        store
            .insert_session(&failed)
            .expect("failed legacy successor");
        assert_eq!(
            store
                .find_published_rotation_successor(predecessor.id)
                .expect("lookup"),
            Some(live.id),
            "the newest non-Failed legacy successor is the tip"
        );
    }
}
