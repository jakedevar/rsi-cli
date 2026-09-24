//! Durable, attributed manager lifecycle commands. V104 is the only storage
//! surface: admission reserves identity, execution claims before effects, and
//! lost execution ownership becomes uncertain rather than being replayed.

use super::Store;
use super::harness_manager_v2::{ManagerAuthorityV2, fingerprint, now, refused};
use super::sessions::historical_session_restore_blocked_on;
use crate::error::Result;
use chrono::Utc;
use rsi_common::harness_manager_v2::*;
use rsi_common::types::{Session, SessionKind, SessionStatus};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;

pub mod fence;
mod operator_delegation;
mod recovery;

const ACTION_KIND: &str = "lifecycle_action";
const CONTEXT_KIND: &str = "lifecycle_context";
const EXECUTION_KIND: &str = "lifecycle_execution";
const MAX_PENDING: i64 = 64;
/// Durable witness that a terminal/idle lead's exact agent-declared program
/// outcome was superseded by an explicit manager recovery (K2, #390).
pub(crate) const LEAD_RETIREMENT_KIND: &str = "lead_continuation_retirement";
/// Settlement outcome codes. The original receipt and evidence are retained;
/// settlement is appended and never re-executes an effect.
pub(crate) const SETTLED_EFFECT_ABSENT: &str = "manager_v2_recovered_effect_absent";
pub(crate) const SETTLED_PAUSE_CONFIRMED: &str = "lead_paused_reconciled";
const UNCERTAIN_RECOVERY_BATCH: i64 = 16;
const MAX_SETTLE_NESTING: usize = 8;
/// Idempotency-key namespace owned by the DB-native review allocator (#674
/// K15A-1). Only the allocator journals keys in it, so an operation linked by
/// `manager_review_assignments.action_operation_id` is a genuine review launch.
pub(super) const REVIEW_ALLOCATION_KEY_PREFIX: &str = "manager-review-allocation:";
/// Alias `o` is an operation row. A blocked or revoked operation whose target
/// session was never created consumed nothing and is not charged; queued,
/// running, succeeded, failed and uncertain operations stay charged. Root
/// successions keep their existing charge (their `manager_root_successions`
/// occurrence is retained across scopes and counted exactly once).
const CREATION_CHARGED: &str = "NOT (o.state IN ('blocked','revoked') AND json_extract(o.payload_json,'$.request.operation.action')!='succeed_manager' AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.id=o.target_session_id))";

/// Which caller runs [`Store::recovery_owner_gate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryOwnerMode {
    /// A manager lifecycle action. Byte-identical to the pre-extraction
    /// `manager_action_human_gate_with_interrupted_resume`.
    ManagerAction { allow_interrupted_resume: bool },
    /// `AgentArchiveChild`: the strictest program mode, with no
    /// interrupted-resume exception, and no hold on the C5 marker that the
    /// archive commit settles itself.
    AgentArchive,
}

// Alias `a` is an operation row. Lost effects fence their still-live lead
// authority (or an installed candidate), not an Epic forever after explicit
// lead repair. Keep the old operation and its evidence intact for inspection.
pub(super) const UNCERTAINTY_CURRENT_AUTHORITY: &str = "(
    json_extract(a.payload_json,'$.request.operation.epic_id') IS NULL OR
    EXISTS(SELECT 1 FROM sessions e JOIN epic_lead_generations g ON g.epic_id=e.id
      WHERE e.id=json_extract(a.payload_json,'$.request.operation.epic_id') AND
      (e.lead_session_id=a.target_session_id OR
       (e.lead_session_id IS json_extract(a.payload_json,'$.request.operation.expected.lead_session_id')
        AND g.generation=json_extract(a.payload_json,'$.request.operation.expected.lead_generation')))))";

/// Coordinator-owned holds are persisted through the shared record helpers.
/// Keys are disjoint so releasing resources cannot clear a human decision.
pub(crate) const MANAGER_ACTION_HOLD_KIND: &str = "lifecycle_hold";
#[derive(Debug, Clone, Copy)]
pub(crate) enum ManagerActionHoldReasonV2 {
    Resources,
    Decision,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagerActionRuntimeHoldV2 {
    pub blocked: bool,
}
pub(crate) fn manager_action_hold_key(
    reason: ManagerActionHoldReasonV2,
    epic: Option<Uuid>,
) -> String {
    let reason = match reason {
        ManagerActionHoldReasonV2::Resources => "resources",
        ManagerActionHoldReasonV2::Decision => "decision",
    };
    format!(
        "{reason}:{}",
        epic.map(|id| id.to_string())
            .unwrap_or_else(|| "project".into())
    )
}

/// Internal attribution; no RPC accepts this enum. Intent dispatch derives the
/// appointment from the project, never borrows a feature lead's identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ManagerActionOriginV2 {
    Agent { caller: Uuid },
    OperatingIntent { project_id: Uuid, intent_id: Uuid },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ManagerActionSourceV2 {
    pub session_id: Uuid,
    pub working_dir: PathBuf,
    pub sandbox_root: Option<PathBuf>,
    pub commit: String,
    pub custody_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub historical_commit: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ManagerActionContextV2 {
    pub origin: ManagerActionOriginV2,
    pub request: AgentManagerControlRequestV2,
    pub target_session_id: Option<Uuid>,
    pub source: Option<ManagerActionSourceV2>,
    pub launch: Option<ManagerLaunchChoiceV2>,
    /// Admission witness: an older explicit resume cannot clear a newer pause.
    #[serde(default)]
    pub manager_pause_version: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagerActionOperationV2 {
    pub project_id: Uuid,
    pub manager_session_id: Uuid,
    pub scope_version: i64,
    pub context: ManagerActionContextV2,
    pub receipt: ManagerActionReceiptV2,
    pub effect_started: bool,
    pub settled_fence: Option<ManagerLeadFenceV2>,
    pub claim_boot_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagerActionClaimV2 {
    pub operation: ManagerActionOperationV2,
    pub boot_id: Uuid,
}

#[derive(Debug, Clone)]
pub(super) struct ManagerActionAdmissionV2 {
    pub target: Option<Session>,
    pub target_session_id: Option<Uuid>,
    pub source: Option<ManagerActionSourceV2>,
    pub launch: Option<ManagerLaunchChoiceV2>,
    pub delay_seconds: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagerCascadeArchiveRecord {
    pub ids: Vec<Uuid>,
    pub completed_at: chrono::DateTime<chrono::FixedOffset>,
}

impl ManagerActionClaimV2 {
    pub(crate) fn id(&self) -> Uuid {
        self.operation.receipt.operation_id
    }
    pub(crate) fn action(&self) -> &ManagerActionV2 {
        &self.operation.context.request.operation
    }
}

pub(crate) fn action_epic(action: &ManagerActionV2) -> Option<Uuid> {
    match action {
        ManagerActionV2::ResumeLead { epic_id, .. }
        | ManagerActionV2::PauseLead { epic_id, .. }
        | ManagerActionV2::RetryLead { epic_id, .. }
        | ManagerActionV2::ReplaceLead { epic_id, .. }
        | ManagerActionV2::AssignLead { epic_id, .. }
        | ManagerActionV2::RetireLeadContinuations { epic_id, .. } => Some(*epic_id),
        _ => None,
    }
}

pub(crate) fn action_fence(action: &ManagerActionV2) -> Option<&ManagerLeadFenceV2> {
    match action {
        ManagerActionV2::ResumeLead { expected, .. }
        | ManagerActionV2::PauseLead { expected, .. }
        | ManagerActionV2::RetryLead { expected, .. }
        | ManagerActionV2::ReplaceLead { expected, .. }
        | ManagerActionV2::AssignLead { expected, .. }
        | ManagerActionV2::RetireLeadContinuations { expected, .. } => Some(expected),
        _ => None,
    }
}

/// `resume_lead` admission for a settled lead applies the same target check as
/// the manager continuation gate (`lifecycle::check_manager_resume_target`).
/// An unsettled lead's status can still change, so the gate decides it at
/// execution, as before.
pub(crate) fn manager_resume_available(lead: &Session) -> Result<()> {
    let settled = matches!(
        lead.status,
        SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
    );
    if settled {
        crate::session::lifecycle::check_manager_resume_target(lead)?;
    }
    Ok(())
}

/// `retry_lead` admits a `Failed` or `Interrupted` lead, and a `Completed` lead
/// that fails `lifecycle::manager_lead_provider_resumable` (the predicate that
/// resume admission and the continuation gate use). Applied at admission and
/// again at execution.
pub(crate) fn manager_retry_admissible(lead: &Session) -> Result<()> {
    match lead.status {
        SessionStatus::Failed | SessionStatus::Interrupted => Ok(()),
        SessionStatus::Completed
            if !crate::session::lifecycle::manager_lead_provider_resumable(lead) =>
        {
            Ok(())
        }
        SessionStatus::Completed => {
            Err(crate::session::lifecycle::manager_refusal_with_next_action(
                "manager_v2_retry_lead_resumable",
                "resume_lead",
            ))
        }
        _ => Err(refused("manager_v2_retry_requires_terminal")),
    }
}

fn state_name(state: ManagerActionStateV2) -> &'static str {
    match state {
        ManagerActionStateV2::Queued => "queued",
        ManagerActionStateV2::Running => "running",
        ManagerActionStateV2::Succeeded => "succeeded",
        ManagerActionStateV2::Failed => "failed",
        ManagerActionStateV2::Blocked => "blocked",
        ManagerActionStateV2::Uncertain => "uncertain",
        ManagerActionStateV2::Revoked => "revoked",
    }
}

impl Store {
    pub(super) fn manager_action_authority(
        &self,
        origin: &ManagerActionOriginV2,
        request: &AgentManagerControlRequestV2,
    ) -> Result<ManagerAuthorityV2> {
        if matches!(request.operation, ManagerActionV2::SucceedManager { .. })
            && !matches!(origin, ManagerActionOriginV2::Agent { .. })
        {
            return Err(refused("manager_succession_agent_origin_required"));
        }
        match origin {
            ManagerActionOriginV2::Agent { caller } => self.manager_v2_authorize(
                *caller,
                &request.fence,
                Some(request.operation.capability()),
            ),
            ManagerActionOriginV2::OperatingIntent {
                project_id,
                intent_id,
            } => {
                if intent_id.is_nil() {
                    return Err(refused("manager_v2_invalid_intent"));
                }
                let config = self
                    .get_harness_manager(*project_id)?
                    .ok_or_else(|| refused("manager_v2_grant_required"))?;
                // This resolves the live appointment using the shared scope validator.
                // Attribution remains daemon-owned (actor_session_id=NULL).
                let authority = self.manager_v2_authorize(
                    config
                        .current_session_id
                        .ok_or_else(|| refused("manager_v2_scope_changed"))?,
                    &request.fence,
                    Some(request.operation.capability()),
                )?;
                if authority.grant.policy.mode != ManagerOperatingModeV2::Execute {
                    return Err(refused("manager_v2_execute_required"));
                }
                Ok(authority)
            }
        }
    }

    /// Read-only exact fence for progress/decision/coordinator observations.
    pub(crate) fn manager_action_lead_fence(&self, epic_id: Uuid) -> Result<ManagerLeadFenceV2> {
        let epic = self
            .get_session(epic_id)?
            .ok_or_else(|| refused("manager_v2_epic_unavailable"))?;
        if epic.session_kind != SessionKind::Epic {
            return Err(refused("manager_v2_epic_unavailable"));
        }
        let generation: i64 = self.conn.query_row(
            "SELECT generation FROM epic_lead_generations WHERE epic_id=?1",
            [epic_id.to_string()],
            |r| r.get(0),
        )?;
        let (event_sequence, custody_generation) = if let Some(lead) = epic.lead_session_id {
            let sequence = self.conn.query_row(
                "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
                [lead.to_string()],
                |r| r.get(0),
            )?;
            let custody = self.manager_action_custody_generation(lead)?;
            (sequence, custody)
        } else {
            (0, None)
        };
        Ok(ManagerLeadFenceV2 {
            lead_session_id: epic.lead_session_id,
            lead_generation: generation,
            event_sequence,
            custody_generation,
        })
    }

    pub(super) fn manager_action_custody_generation(&self, session: Uuid) -> Result<Option<i64>> {
        // The persisted projection is a witness, never authority to execute.
        Ok(self.conn.query_row("SELECT r.generation FROM sessions s LEFT JOIN sandbox_custody_roots r ON r.custody_id=s.sandbox_custody_id WHERE s.id=?1", [session.to_string()], |r| r.get(0))?)
    }

    fn manager_action_check_fence(&self, epic: Uuid, expected: &ManagerLeadFenceV2) -> Result<()> {
        let current = self.manager_action_lead_fence(epic)?;
        if expected.lead_generation <= 0
            || expected.event_sequence < 0
            || current.lead_session_id != expected.lead_session_id
            || current.lead_generation != expected.lead_generation
            || current.event_sequence != expected.event_sequence
            || current.custody_generation != expected.custody_generation
        {
            return Err(refused("manager_v2_lead_changed"));
        }
        Ok(())
    }

    pub(super) fn manager_action_target(
        &self,
        authority: &ManagerAuthorityV2,
        action: &ManagerActionV2,
        check_version: bool,
    ) -> Result<Option<Session>> {
        use ManagerActionV2::*;
        if let SettleUncertainAction {
            operation_id,
            expected_row_version,
        } = action
        {
            self.manager_settle_target(
                authority,
                *operation_id,
                *expected_row_version,
                check_version,
            )?;
            return Ok(None);
        }
        if let Some(epic) = action_epic(action) {
            let container = self.manager_v2_require_epic(authority, epic)?;
            if check_version {
                self.manager_action_check_fence(
                    epic,
                    action_fence(action).expect("lead action fence"),
                )?;
                self.reject_nonterminal_agent_successor_epic_lead_mutation(epic)?;
            }
            if let AssignLead {
                session_id: Some(id),
                ..
            } = action
            {
                let candidate = self
                    .get_session(*id)?
                    .ok_or_else(|| refused("manager_v2_candidate_unavailable"))?;
                if candidate.parent_id != Some(epic)
                    || candidate.project_id != Some(authority.config.project_id)
                    || !rsi_common::is_leaf_kind(candidate.session_kind)
                    || !rsi_common::legal_children(Some(SessionKind::Epic))
                        .contains(&candidate.session_kind)
                    || matches!(
                        candidate.status,
                        SessionStatus::Archived | SessionStatus::Deleted
                    )
                {
                    return Err(refused("manager_v2_candidate_out_of_scope"));
                }
            }
            return container
                .lead_session_id
                .map(|id| {
                    let lead = self
                        .get_session(id)?
                        .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
                    if lead.parent_id != Some(epic)
                        || lead.project_id != Some(authority.config.project_id)
                        || !rsi_common::is_leaf_kind(lead.session_kind)
                        || !rsi_common::legal_children(Some(SessionKind::Epic))
                            .contains(&lead.session_kind)
                        || matches!(
                            lead.status,
                            SessionStatus::Archived | SessionStatus::Deleted
                        )
                    {
                        return Err(refused("manager_v2_lead_out_of_scope"));
                    }
                    Ok(lead)
                })
                .transpose();
        }
        match action {
            SucceedManager { expected, .. } => {
                let observed = self.manager_succession_observation(authority.caller)?;
                if check_version && observed.expected != *expected {
                    return Err(refused("manager_succession_authority_changed"));
                }
                self.get_session(authority.caller)
            }
            CreateContainer {
                parent_id, kind, ..
            } => {
                let parent = parent_id
                    .map(|id| self.manager_v2_require_container(authority, id, false))
                    .transpose()?;
                if !rsi_common::is_container_kind(*kind)
                    || !rsi_common::legal_children(parent.as_ref().map(|s| s.session_kind))
                        .contains(kind)
                    || (parent.is_none()
                        && (!authority.grant.policy.allow_create_groups
                            || *kind != SessionKind::Group))
                {
                    return Err(refused("manager_v2_container_out_of_scope"));
                }
                Ok(parent)
            }
            CreateSession {
                parent_id, kind, ..
            } => {
                let parent = self.manager_v2_require_container(authority, *parent_id, false)?;
                if !rsi_common::is_leaf_kind(*kind)
                    || !rsi_common::legal_children(Some(parent.session_kind)).contains(kind)
                {
                    return Err(refused("manager_v2_illegal_child"));
                }
                Ok(Some(parent))
            }
            UpdateContainer {
                container_id,
                expected_updated_at,
                ..
            }
            | ArchiveContainer {
                container_id,
                expected_updated_at,
            }
            | DeleteContainer {
                container_id,
                expected_updated_at,
            }
            | RestoreContainer {
                container_id,
                expected_updated_at,
            } => {
                // Replay may inspect a retained row after its successful retirement.
                let container =
                    self.manager_v2_require_container(authority, *container_id, true)?;
                if check_version {
                    if container.updated_at != *expected_updated_at {
                        return Err(refused("manager_v2_container_changed"));
                    }
                    let restore = matches!(action, RestoreContainer { .. });
                    if restore
                        != matches!(
                            container.status,
                            SessionStatus::Archived | SessionStatus::Deleted
                        )
                    {
                        return Err(refused("manager_v2_container_state_changed"));
                    }
                    if matches!(action, DeleteContainer { .. }) {
                        let nonempty: bool = self.conn.query_row(
                            "SELECT EXISTS(SELECT 1 FROM sessions WHERE parent_id=?1)",
                            [container_id.to_string()],
                            |r| r.get(0),
                        )?;
                        if nonempty || container.lead_session_id.is_some() {
                            return Err(refused("container_not_empty"));
                        }
                    }
                    if matches!(action, ArchiveContainer { .. }) {
                        self.manager_action_validate_archive_cascade(*container_id)?;
                    }
                    if matches!(action, RestoreContainer { .. }) {
                        self.manager_action_validate_restore_container(&container, false)?;
                    }
                }
                Ok(Some(container))
            }
            ArchiveSession {
                session_id,
                expected_updated_at,
            }
            | RestoreSession {
                session_id,
                expected_updated_at,
            }
            | UpdateSession {
                session_id,
                expected_updated_at,
                ..
            } => {
                // Replay may inspect a retained row after its archive/restore.
                let (session, epic) = self.manager_action_session_target(authority, *session_id)?;
                if check_version {
                    let policy = &authority.grant.policy;
                    if policy.mode != ManagerOperatingModeV2::Execute {
                        return Err(refused("manager_v2_execute_required"));
                    }
                    if policy.paused || policy.paused_epic_ids.contains(&epic) {
                        return Err(refused("manager_v2_policy_paused"));
                    }
                    if session.updated_at != *expected_updated_at {
                        return Err(refused("manager_v2_session_changed"));
                    }
                    match action {
                        ArchiveSession { .. } => {
                            self.manager_action_validate_archive_session(&session)?;
                        }
                        RestoreSession { .. } => {
                            if session.status != SessionStatus::Archived {
                                return Err(refused("manager_v2_session_state_changed"));
                            }
                            if historical_session_restore_blocked_on(&self.conn, session.id)? {
                                return Err(refused("manager_v2_historical_restore_refused"));
                            }
                        }
                        UpdateSession { patch, .. } => {
                            if matches!(
                                session.status,
                                SessionStatus::Archived | SessionStatus::Deleted
                            ) {
                                return Err(refused("manager_v2_session_state_changed"));
                            }
                            self.manager_action_session_patch_tags(authority, patch)?;
                        }
                        _ => unreachable!("session housekeeping arm"),
                    }
                }
                Ok(Some(session))
            }
            OperatorCall { call, expected } => self.manager_action_operator_target(
                authority,
                call,
                expected.as_ref(),
                check_version,
            ),
            _ => unreachable!("lead actions handled above"),
        }
    }

    /// One scoped leaf reached through the shared manager reach definition
    /// (`manager_session_scope`). Containers stay under Topology's actions.
    fn manager_action_session_target(
        &self,
        authority: &ManagerAuthorityV2,
        id: Uuid,
    ) -> Result<(Session, Uuid)> {
        let scope = self
            .manager_session_scope(authority.caller, id)?
            .filter(|scope| scope.config.project_id == authority.config.project_id)
            .ok_or_else(|| refused("manager_v2_session_out_of_scope"))?;
        if !rsi_common::is_leaf_kind(scope.target.session_kind) {
            return Err(refused("manager_v2_leaf_required"));
        }
        Ok((scope.target, scope.epic_id))
    }

    /// Nearest Epic at or above a session, for notice and gate attribution.
    pub(crate) fn manager_action_session_epic(&self, id: Uuid) -> Result<Option<Uuid>> {
        let mut cursor = Some(id);
        for _ in 0..32 {
            let Some(current) = cursor else { break };
            let Some(row) = self.get_session(current)? else {
                break;
            };
            if row.session_kind == SessionKind::Epic {
                return Ok(Some(row.id));
            }
            cursor = row.parent_id;
        }
        Ok(None)
    }

    /// A single terminal leaf only: trees use the container cascade, and a
    /// current lead must be replaced/unassigned first. The operator archive
    /// path clears lead pointers; the manager path refuses instead.
    fn manager_action_validate_archive_session(&self, session: &Session) -> Result<()> {
        match session.status {
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted => {}
            SessionStatus::Archived | SessionStatus::Deleted => {
                return Err(refused("manager_v2_session_state_changed"));
            }
            _ => return Err(refused("manager_v2_session_not_terminal")),
        }
        for id in self
            .manager_action_descendant_ids(session.id)?
            .into_iter()
            .skip(1)
        {
            let status: Option<String> = self
                .conn
                .query_row(
                    "SELECT status FROM sessions WHERE id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if !matches!(status.as_deref(), Some("Archived" | "Deleted")) {
                return Err(refused("manager_v2_session_has_descendants"));
            }
        }
        let lead: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE lead_session_id=?1
               AND status NOT IN ('Archived','Deleted'))",
            [session.id.to_string()],
            |row| row.get(0),
        )?;
        if lead {
            return Err(refused("manager_v2_session_is_lead"));
        }
        self.manager_action_human_gate(session.id)
    }

    /// Shape, tag normalization (exactly `update_session_tags`: normalize,
    /// sort, dedup) and label existence. Returns the normalized tag set.
    fn manager_action_session_patch_tags(
        &self,
        authority: &ManagerAuthorityV2,
        patch: &ManagerSessionPatchV2,
    ) -> Result<Option<Vec<String>>> {
        patch.validate().map_err(refused)?;
        if let Some(ManagerFieldPatchV2::Set(label)) = &patch.label {
            let label = self
                .get_label(*label)?
                .ok_or_else(|| refused("manager_v2_label_unavailable"))?;
            if label
                .project_id
                .is_some_and(|project| project != authority.config.project_id)
            {
                return Err(refused("manager_v2_label_unavailable"));
            }
        }
        patch
            .tags
            .as_ref()
            .map(|tags| {
                let mut normalized = tags
                    .iter()
                    .map(|tag| {
                        rsi_common::normalize_tag(tag)
                            .map_err(|_| refused("manager_v2_invalid_tags"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                normalized.sort();
                normalized.dedup();
                Ok(normalized)
            })
            .transpose()
    }

    /// Durable operator ownership independent of the manager's scope. This
    /// marker can only be cleared by an explicit operator continuation.
    pub(crate) fn record_manager_operator_pause(&self, session: Uuid, paused: bool) -> Result<()> {
        let key = format!("manager_operator_pause:{session}");
        self.conn.execute("INSERT INTO daemon_settings(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",params![key,if paused {"true"} else {"false"},now()])?;
        Ok(())
    }

    pub(crate) fn manager_action_operator_paused(&self, target: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM daemon_settings WHERE key=?1 AND value<>'false')",
            [format!("manager_operator_pause:{target}")],
            |r| r.get(0),
        )?)
    }

    pub(crate) fn manager_action_human_gate(&self, target: Uuid) -> Result<()> {
        self.manager_action_human_gate_with_interrupted_resume(target, false)
    }

    fn manager_action_descendant_ids(&self, root: Uuid) -> Result<Vec<Uuid>> {
        let mut ids = vec![root];
        let mut pending = vec![root];
        let mut seen = std::collections::HashSet::from([root]);
        while let Some(parent) = pending.pop() {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM sessions WHERE parent_id=?1 ORDER BY id LIMIT 513")?;
            let children = stmt
                .query_map(params![parent.to_string()], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if children.len() > 512 || ids.len().saturating_add(children.len()) > 512 {
                return Err(refused("manager_v2_cascade_too_large"));
            }
            for raw in children {
                let child = Uuid::parse_str(&raw)
                    .map_err(|_| refused("manager_v2_invalid_cascade_tree"))?;
                if !seen.insert(child) {
                    return Err(refused("manager_v2_invalid_cascade_tree"));
                }
                ids.push(child);
                pending.push(child);
            }
        }
        Ok(ids)
    }

    pub(crate) fn manager_action_container_tree_ids(&self, root: Uuid) -> Result<Vec<Uuid>> {
        self.manager_action_descendant_ids(root)
    }

    fn manager_action_validate_archive_cascade(&self, root: Uuid) -> Result<Vec<(Uuid, bool)>> {
        let ids = self.manager_action_descendant_ids(root)?;
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let session = self
                .get_session(id)?
                .ok_or_else(|| refused("manager_v2_container_changed"))?;
            let archived = session.status == SessionStatus::Archived;
            if !matches!(
                session.status,
                SessionStatus::Completed
                    | SessionStatus::Failed
                    | SessionStatus::Interrupted
                    | SessionStatus::Archived
            ) {
                return Err(refused("container_not_terminal"));
            }
            if !archived && rsi_common::is_leaf_kind(session.session_kind) {
                self.manager_action_human_gate(id)?;
            }
            result.push((id, archived));
        }
        Ok(result)
    }

    /// IDs newly archived by the current successful cascade archive for this
    /// container. A later manager restore consumes the record; an external row
    /// update also invalidates it by advancing `sessions.updated_at`.
    pub(crate) fn manager_action_cascade_archive_ids(
        &self,
        root: Uuid,
    ) -> Result<Option<ManagerCascadeArchiveRecord>> {
        let latest: Option<(String, Option<String>, String, String)> = self
            .conn
            .query_row(
                "SELECT json_extract(o.payload_json,'$.request.operation.action'),
                        json_extract(o.outcome_json,'$.execution.cascade_archived_ids'),
                        s.updated_at,o.updated_at
             FROM harness_manager_v2_operations o
             JOIN sessions s ON s.id=o.target_session_id
             WHERE o.kind='lifecycle_action' AND o.state='succeeded'
               AND o.target_session_id=?1
               AND json_extract(o.payload_json,'$.request.operation.action')
                   IN ('archive_container','restore_container')
             ORDER BY o.updated_at DESC,o.rowid DESC LIMIT 1",
                [root.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((latest_action, evidence, container_updated_at, operation_completed_at)) = latest
        else {
            return Ok(None);
        };
        if latest_action != "archive_container" {
            return Ok(None);
        }
        let updated_at = chrono::DateTime::parse_from_rfc3339(&container_updated_at)
            .map_err(|_| refused("manager_v2_invalid_cascade_timestamp"))?;
        let completed_at = chrono::DateTime::parse_from_rfc3339(&operation_completed_at)
            .map_err(|_| refused("manager_v2_invalid_cascade_timestamp"))?;
        if updated_at > completed_at {
            return Ok(None);
        }
        let ids = evidence
            .map(|json| {
                let values: Vec<String> = serde_json::from_str(&json)?;
                values
                    .into_iter()
                    .map(|value| {
                        Uuid::parse_str(&value)
                            .map_err(|_| refused("manager_v2_invalid_cascade_evidence"))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        Ok(ids.map(|ids| ManagerCascadeArchiveRecord { ids, completed_at }))
    }

    /// Shared restore check. Admission checks container occupancy; inspection
    /// and execution also check recorded cascade members. Existing actions keep
    /// stale-member failures at execution, after the queued receipt is durable.
    pub(super) fn manager_action_validate_restore_container(
        &self,
        container: &Session,
        check_cascade_members: bool,
    ) -> Result<Option<ManagerCascadeArchiveRecord>> {
        let recorded = self.manager_action_cascade_archive_ids(container.id)?;
        if recorded.is_none() {
            let nonempty: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE parent_id=?1)",
                [container.id.to_string()],
                |row| row.get(0),
            )?;
            if nonempty || container.lead_session_id.is_some() {
                return Err(refused("container_not_empty"));
            }
        }
        if let Some(record) = recorded.as_ref().filter(|_| check_cascade_members) {
            let mut members = record.ids.clone();
            if !members.contains(&container.id) {
                members.push(container.id);
            }
            if members.len() > 512 {
                return Err(refused("manager_v2_cascade_too_large"));
            }
            for member in members {
                let row: Option<(String, String)> = self
                    .conn
                    .query_row(
                        "SELECT status,updated_at FROM sessions WHERE id=?1",
                        [member.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let Some((status, updated_at)) = row else {
                    return Err(refused("manager_v2_cascade_record_stale"));
                };
                let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at)
                    .map_err(|_| refused("manager_v2_invalid_cascade_timestamp"))?;
                if updated_at > record.completed_at {
                    return Err(refused("manager_v2_cascade_record_stale"));
                }
                if status == "Archived"
                    && historical_session_restore_blocked_on(&self.conn, member)?
                {
                    return Err(refused("manager_v2_historical_restore_refused"));
                }
            }
        }
        Ok(recorded)
    }

    /// Genuine operator/daemon ownership that no manager recovery may clear:
    /// questions, approvals, operator pause, C5 autofile and capacity owners.
    /// Continuation wakes and program declarations are checked separately.
    pub(crate) fn manager_action_operator_gate(&self, target: Uuid) -> Result<()> {
        let held: bool = self.conn.query_row(
            "SELECT pending_question_json IS NOT NULL OR pending_archive=1 OR status='WaitingApproval'
             OR EXISTS(SELECT 1 FROM approvals a WHERE session_id=?1 AND status='Pending' AND NOT EXISTS(SELECT 1 FROM appserver_approval_publications p WHERE p.approval_id=a.id AND (p.closure_state='closed' OR p.state='superseded')))
             OR EXISTS(SELECT 1 FROM appserver_approval_publications WHERE session_id=?1 AND closure_state<>'closed' AND state<>'superseded')
             OR EXISTS(SELECT 1 FROM daemon_settings WHERE key=?3 OR (key=?2 AND value<>'false'))
             OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents WHERE state='open' AND controller_session_id=?1)
             OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents i JOIN model_invocations m ON m.id=i.last_capacity_model_invocation_id WHERE i.state='open' AND m.session_id=?1)
             FROM sessions WHERE id=?1", params![target.to_string(), format!("manager_operator_pause:{target}"), super::daemon_settings::c5_autofile_pending_key(target)], |r| r.get(0))?;
        if held {
            return Err(refused("manager_v2_human_or_recovery_owner"));
        }
        Ok(())
    }

    /// The exception is derived only by the live action gate below. Default
    /// callers (automatic intent, assignment and approval checks) retain it off.
    pub(crate) fn manager_action_human_gate_with_interrupted_resume(
        &self,
        target: Uuid,
        allow_interrupted_resume: bool,
    ) -> Result<()> {
        self.recovery_owner_gate(
            target,
            RecoveryOwnerMode::ManagerAction {
                allow_interrupted_resume,
            },
        )
    }

    /// The one recovery-owner check shared by manager lifecycle actions and
    /// `AgentArchiveChild`: the held-row SQL plus the program gate.
    ///
    /// Only the C5 autofile-marker clause is mode-bound. A manager action
    /// treats a pending marker as the settlement owner; an agent archive
    /// settles that marker in its own commit instead, so its mode does not
    /// hold on it. Every other clause, and the program gate, applies to both
    /// modes. It reads through `self.conn`, so a caller holding an open
    /// transaction on that connection observes its own writes.
    pub(crate) fn recovery_owner_gate(&self, target: Uuid, mode: RecoveryOwnerMode) -> Result<()> {
        let (hold_on_c5_marker, allow_interrupted_resume) = match mode {
            RecoveryOwnerMode::ManagerAction {
                allow_interrupted_resume,
            } => (true, allow_interrupted_resume),
            RecoveryOwnerMode::AgentArchive => (false, false),
        };
        let held: bool = self.conn.query_row(
            "SELECT pending_question_json IS NOT NULL OR pending_archive=1 OR status='WaitingApproval'
             OR EXISTS(SELECT 1 FROM approvals a WHERE session_id=?1 AND status='Pending' AND NOT EXISTS(SELECT 1 FROM appserver_approval_publications p WHERE p.approval_id=a.id AND (p.closure_state='closed' OR p.state='superseded')))
             OR EXISTS(SELECT 1 FROM appserver_approval_publications WHERE session_id=?1 AND closure_state<>'closed' AND state<>'superseded')
             OR EXISTS(SELECT 1 FROM daemon_settings WHERE (?5 AND key=?3) OR (key=?2 AND value<>'false'))
             OR EXISTS(SELECT 1 FROM scheduled_jobs WHERE enabled=1 AND wake_mode='resume' AND wake_session_id=?1 AND id<>?4)
             OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents WHERE state='open' AND controller_session_id=?1)
             OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents i JOIN model_invocations m ON m.id=i.last_capacity_model_invocation_id WHERE i.state='open' AND m.session_id=?1)
             FROM sessions WHERE id=?1", params![target.to_string(), format!("manager_operator_pause:{target}"), super::daemon_settings::c5_autofile_pending_key(target), crate::session::harness::tools::schedule_wake::deterministic_program_guard_job_id(target).to_string(), hold_on_c5_marker], |r| r.get(0))?;
        if held {
            return Err(refused("manager_v2_human_or_recovery_owner"));
        }
        crate::session::lifecycle::check_manager_action_program_gate(
            self,
            target,
            allow_interrupted_resume,
        )?;
        Ok(())
    }

    pub(super) fn manager_action_resources(
        &self,
        authority: &ManagerAuthorityV2,
        launch: &ManagerLaunchChoiceV2,
        exclude: Option<Uuid>,
    ) -> Result<()> {
        let policy = &authority.grant.policy;
        launch.validate().map_err(refused)?;
        if !policy.allowed_launches.is_empty()
            && !policy.allowed_launches.iter().any(|l| {
                l.provider == launch.provider
                    && l.model == launch.model
                    && l.effort == launch.effort
            })
        {
            return Err(refused("manager_v2_launch_not_granted"));
        }
        let epic = exclude
            .map(|id| self.get_session(id))
            .transpose()?
            .flatten()
            .and_then(|s| s.parent_id)
            .filter(|id| authority.config.epic_ids.contains(id));
        self.manager_v2_resource_gate(&authority.config, epic, launch.provider, exclude)
    }

    /// Lifetime-within-scope creation usage charged against
    /// `max_created_{containers,sessions}` (#674). Reservations count as well
    /// as committed creations, and policy edits do not reset it. DB-native
    /// review launches are not charged, and a blocked or revoked operation
    /// that never created its target is free.
    pub(crate) fn manager_v2_created_usage(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        container: bool,
    ) -> Result<i64> {
        let count: i64 = self.conn.query_row(&format!("SELECT count(*) FROM harness_manager_v2_operations o WHERE o.project_id=?1 AND o.manager_session_id=?2 AND o.scope_version=?3 AND o.kind=?4 AND json_extract(o.payload_json,'$.request.operation.action') IN (SELECT value FROM json_each(?5)) AND NOT EXISTS(SELECT 1 FROM manager_review_assignments r WHERE r.action_operation_id=o.id) AND {CREATION_CHARGED}"), params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,ACTION_KIND,if container { "[\"create_container\"]" } else { "[\"create_session\",\"replace_lead\",\"retry_lead\",\"succeed_manager\"]" }], |r|r.get(0))?;
        if container {
            return Ok(count);
        }
        // Root occurrences retain their creation charge across scope/appointment
        // changes. They are not a recovery and are counted exactly once.
        let retained: i64 = self.conn.query_row(
            "SELECT count(*) FROM manager_root_successions WHERE project_id=?1 AND NOT (manager_session_id=?2 AND scope_version=?3)",
            params![config.project_id.to_string(), config.manager_session_id.to_string(), config.row_version],
            |r| r.get(0),
        )?;
        Ok(count + retained)
    }

    /// Test fixture: journal a lifecycle operation in an exact state for the
    /// current seat and scope without running admission or execution.
    #[cfg(test)]
    pub(crate) fn seed_manager_action_for_test(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        operation: ManagerActionV2,
        state: ManagerActionStateV2,
        target_session_id: Option<Uuid>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let receipt = ManagerActionReceiptV2 {
            operation_id: id,
            state,
            action_kind: operation.action_kind(),
            target_type: operation.target_type(),
            row_version: 1,
            target_session_id,
            outcome: None,
            result: None,
            deduplicated: false,
            operator_result: None,
        };
        let request = AgentManagerControlRequestV2 {
            fence: ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version: 1,
            },
            idempotency_key: format!("seeded:{id}"),
            operation,
        };
        let origin = ManagerActionOriginV2::Agent {
            caller: config.manager_session_id,
        };
        let payload = json!({"origin":origin,"request":request});
        let stamp = now();
        self.conn.execute("INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,scope_version,policy_version,actor_session_id,idempotency_key,fingerprint,kind,payload_json,state,row_version,target_session_id,outcome_json,not_before,created_at,updated_at) VALUES(?1,?2,?3,?4,1,?3,?5,?6,?7,?8,?9,1,?10,?11,?12,?12,?12)", params![id.to_string(),config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,request.idempotency_key,fingerprint(&payload)?,ACTION_KIND,serde_json::to_string(&payload)?,state_name(state),target_session_id.map(|v|v.to_string()),serde_json::to_string(&receipt)?,stamp])?;
        Ok(id)
    }

    pub(super) fn manager_action_creation_budget(
        &self,
        authority: &ManagerAuthorityV2,
        container: bool,
        epic: bool,
    ) -> Result<()> {
        let count = self.manager_v2_created_usage(&authority.config, container)?;
        let cap = if container {
            authority.grant.policy.max_created_containers
        } else {
            authority.grant.policy.max_created_sessions
        };
        if count >= i64::from(cap) {
            return Err(refused("manager_v2_creation_limit"));
        }
        if epic && !authority.config.has_dynamic_scope() {
            let pending: i64 = self.conn.query_row("SELECT count(*) FROM harness_manager_v2_operations WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND state IN ('queued','running','uncertain') AND json_extract(payload_json,'$.request.operation.action')='create_container' AND json_extract(payload_json,'$.request.operation.kind')='Epic'", params![authority.config.project_id.to_string(),authority.config.manager_session_id.to_string(),authority.config.row_version,ACTION_KIND], |r|r.get(0))?;
            if authority.config.epic_ids.len() as i64 + pending >= 32 {
                return Err(refused("manager_v2_scope_limit"));
            }
        }
        Ok(())
    }

    pub(super) fn manager_action_freeze_source(
        &self,
        source: &Session,
    ) -> Result<ManagerActionSourceV2> {
        use crate::sandbox::custody::{CustodyClassification, CustodyService};
        let root = match CustodyService::classify(source)? {
            CustodyClassification::OrdinaryUnsandboxed => {
                CustodyService::authorize_ordinary(source)?;
                source.working_dir.clone()
            }
            CustodyClassification::RequiresPersistedAuthentication => {
                // Execution authenticates this projection against the daemon's
                // allocator and the filesystem before consuming the frozen OID.
                self.live_custody_for_session(source.id)?;
                source
                    .sandbox_root
                    .clone()
                    .ok_or_else(|| refused("manager_v2_source_custody_changed"))?
            }
        };
        let (clean, commit) = crate::sandbox::git_worktree::observe_clean_head_bounded(&root)?;
        if !clean {
            return Err(refused("manager_v2_source_worktree_dirty"));
        }
        Ok(ManagerActionSourceV2 {
            session_id: source.id,
            working_dir: source.working_dir.clone(),
            sandbox_root: source.sandbox_root.clone(),
            commit,
            custody_generation: self.manager_action_custody_generation(source.id)?,
            historical_commit: false,
        })
    }

    fn manager_action_historical_source(
        &self,
        source: &Session,
        expected_commit: &str,
    ) -> Result<ManagerActionSourceV2> {
        use crate::sandbox::custody::{CustodyClassification, CustodyService};
        if expected_commit.len() != 40
            || expected_commit
                .bytes()
                .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
        {
            return Err(refused("manager_review_source_changed"));
        }
        match CustodyService::classify(source)
            .map_err(|_| refused("manager_review_author_unavailable"))?
        {
            CustodyClassification::OrdinaryUnsandboxed => {
                CustodyService::authorize_ordinary(source)
                    .map_err(|_| refused("manager_review_author_unavailable"))?;
            }
            CustodyClassification::RequiresPersistedAuthentication => {
                self.live_custody_for_session(source.id)
                    .map_err(|_| refused("manager_review_author_unavailable"))?;
            }
        }
        Ok(ManagerActionSourceV2 {
            session_id: source.id,
            working_dir: source.working_dir.clone(),
            sandbox_root: source.sandbox_root.clone(),
            commit: expected_commit.to_string(),
            custody_generation: self
                .manager_action_custody_generation(source.id)
                .map_err(|_| refused("manager_review_author_unavailable"))?,
            historical_commit: true,
        })
    }

    pub(super) fn manager_action_admission(
        &self,
        authority: &ManagerAuthorityV2,
        origin: &ManagerActionOriginV2,
        request: &AgentManagerControlRequestV2,
        allocate_target_identity: bool,
        check_resources: bool,
        review_source: Option<&ManagerActionSourceV2>,
    ) -> Result<ManagerActionAdmissionV2> {
        use ManagerActionV2::*;

        let target = self.manager_action_target(authority, &request.operation, true)?;
        let pending: i64 = self.conn.query_row(&format!("SELECT count(*) FROM harness_manager_v2_operations a WHERE project_id=?1 AND (state IN ('queued','running') OR (state='uncertain' AND {UNCERTAINTY_CURRENT_AUTHORITY})) AND kind=?2"), params![authority.config.project_id.to_string(),ACTION_KIND], |r|r.get(0))?;
        if pending >= MAX_PENDING {
            return Err(refused("manager_v2_action_queue_full"));
        }
        let mut launch = None;
        let mut source = None;
        let mut id = target.as_ref().map(|session| session.id);
        let mut delay_seconds = if matches!(origin, ManagerActionOriginV2::OperatingIntent { .. }) {
            authority.grant.policy.retry_delay_seconds
        } else {
            0
        };
        match &request.operation {
            CreateContainer {
                name, tags, kind, ..
            } => {
                text(name, 512).map_err(refused)?;
                if tags.is_empty() || tags.len() > 32 {
                    return Err(refused("manager_v2_invalid_tags"));
                }
                for tag in tags {
                    rsi_common::normalize_tag(tag)
                        .map_err(|_| refused("manager_v2_invalid_tags"))?;
                }
                self.manager_action_creation_budget(authority, true, *kind == SessionKind::Epic)?;
                id = allocate_target_identity.then(Uuid::new_v4);
            }
            UpdateContainer {
                name, description, ..
            } => {
                text(name, 512).map_err(refused)?;
                if let Some(description) = description {
                    if description.len() > 8192 || description.contains('\0') {
                        return Err(refused("manager_v2_invalid_description"));
                    }
                }
            }
            CreateSession {
                query,
                launch: choice,
                ..
            }
            | ReplaceLead {
                query,
                launch: choice,
                ..
            } => {
                text(query, 32768).map_err(refused)?;
                // Only the DB-native review allocation supplies a review
                // source. Reviewer launches are bounded by per-work review
                // rounds and the active-session gate, not the lifetime
                // creation budget (#674, K15a).
                if review_source.is_none() || !matches!(request.operation, CreateSession { .. }) {
                    self.manager_action_creation_budget(authority, false, false)?;
                }
                source = Some(if let Some(source) = review_source {
                    source.clone()
                } else {
                    let source_session = target
                        .as_ref()
                        .ok_or_else(|| refused("manager_v2_source_unavailable"))?;
                    self.manager_action_freeze_source(source_session)?
                });
                launch = Some(choice.clone());
                id = allocate_target_identity.then(Uuid::new_v4);
            }
            ResumeLead { message, .. } | RetryLead { message, .. } => {
                text(message, 32768).map_err(refused)?;
                let lead = target
                    .as_ref()
                    .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
                launch = Some(ManagerLaunchChoiceV2 {
                    provider: lead.provider,
                    model: lead
                        .model
                        .clone()
                        .ok_or_else(|| refused("manager_v2_model_unavailable"))?,
                    effort: lead.effort.clone(),
                });
                if matches!(request.operation, ResumeLead { .. }) {
                    manager_resume_available(lead)?;
                }
                if let RetryLead { launch: choice, .. } = &request.operation {
                    manager_retry_admissible(lead)?;
                    if let Some(choice) = choice {
                        launch = Some(choice.clone());
                    }
                    self.manager_action_creation_budget(authority, false, false)?;
                    source = Some(self.manager_action_freeze_source(lead)?);
                    id = allocate_target_identity.then(Uuid::new_v4);
                    delay_seconds = authority.grant.policy.retry_delay_seconds;
                    let epic = action_epic(&request.operation).expect("retry epic");
                    let attempts:i64=self.conn.query_row("SELECT count(*) FROM harness_manager_v2_operations WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND json_extract(payload_json,'$.request.operation.action')='retry_lead' AND json_extract(payload_json,'$.request.operation.epic_id')=?5",params![authority.config.project_id.to_string(),authority.config.manager_session_id.to_string(),authority.config.row_version,ACTION_KIND,epic.to_string()],|r|r.get(0))?;
                    if attempts >= i64::from(authority.grant.policy.max_recovery_attempts) {
                        return Err(refused("manager_v2_retry_budget_exhausted"));
                    }
                }
            }
            PauseLead { reason, .. } => {
                text(reason, 8192).map_err(refused)?;
                if target.is_none() {
                    return Err(refused("manager_v2_lead_unavailable"));
                }
            }
            AssignLead { session_id, .. } => {
                id = *session_id;
            }
            OperatorCall { call, .. } => {
                id = call.typed().map_err(refused)?.session_id();
            }
            RetireLeadContinuations { .. } => {
                // Refuse before queueing: genuine operator gates are never
                // superseded by a manager recovery.
                let lead = target
                    .as_ref()
                    .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
                self.manager_action_operator_gate(lead.id)?;
            }
            _ => {}
        }
        if check_resources && let Some(choice) = &launch {
            self.manager_action_resources(
                authority,
                choice,
                target
                    .as_ref()
                    .filter(|_| {
                        matches!(
                            request.operation,
                            ResumeLead { .. } | ReplaceLead { .. } | RetryLead { .. }
                        )
                    })
                    .map(|session| session.id),
            )?;
        }
        Ok(ManagerActionAdmissionV2 {
            target,
            target_session_id: id,
            source,
            launch,
            delay_seconds,
        })
    }

    pub(crate) fn enqueue_manager_action(
        &self,
        origin: ManagerActionOriginV2,
        request: AgentManagerControlRequestV2,
    ) -> Result<ManagerActionReceiptV2> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let receipt = self.enqueue_manager_action_on(origin, request)?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(super) fn enqueue_manager_action_on(
        &self,
        origin: ManagerActionOriginV2,
        request: AgentManagerControlRequestV2,
    ) -> Result<ManagerActionReceiptV2> {
        self.enqueue_manager_action_with_source_on(origin, request, None)
    }

    /// Review allocation uses the existing lifecycle journal and launch path,
    /// but forks from the work author's exact stored source instead of the
    /// Epic container's mutable working directory.
    pub(crate) fn enqueue_manager_review_session_on(
        &self,
        origin: ManagerActionOriginV2,
        request: AgentManagerControlRequestV2,
        source_session: &Session,
        expected_source: &str,
    ) -> Result<ManagerActionReceiptV2> {
        if !matches!(request.operation, ManagerActionV2::CreateSession { .. }) {
            return Err(refused("manager_review_launch_action_required"));
        }
        let source = self.manager_action_historical_source(source_session, expected_source)?;
        self.enqueue_manager_action_with_source_on(origin, request, Some(&source))
    }

    fn enqueue_manager_action_with_source_on(
        &self,
        origin: ManagerActionOriginV2,
        request: AgentManagerControlRequestV2,
        review_source: Option<&ManagerActionSourceV2>,
    ) -> Result<ManagerActionReceiptV2> {
        use ManagerActionV2::*;
        if matches!(request.operation, SucceedManager { .. }) {
            // The runtime routes this variant through enqueue_manager_succession
            // with a private authenticated Git/custody proof.
            return Err(refused("manager_succession_authenticated_source_required"));
        }
        text(&request.idempotency_key, 128).map_err(refused)?;
        // K15A-1: the review allocator's key namespace is reserved. Manager
        // control and prepared commits (review_source None) cannot journal an
        // operation the allocator would later adopt and exclude from the
        // creation budget.
        if review_source.is_none()
            && request
                .idempotency_key
                .starts_with(REVIEW_ALLOCATION_KEY_PREFIX)
        {
            return Err(refused("manager_v2_reserved_idempotency_key"));
        }
        let authority = self.manager_action_authority(&origin, &request)?;
        self.manager_action_target(&authority, &request.operation, false)?;
        // The allocator journals and links its operation in one IMMEDIATE
        // transaction, so a legitimate retry always journals fresh. Any
        // existing operation under its key was not created by it: never adopt.
        if review_source.is_some() {
            let existing: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_operations WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND idempotency_key=?4)",
                params![
                    authority.config.project_id.to_string(),
                    authority.config.manager_session_id.to_string(),
                    authority.config.row_version,
                    request.idempotency_key
                ],
                |r| r.get(0),
            )?;
            if existing {
                return Err(refused("manager_review_allocation_conflict"));
            }
        }
        let payload = json!({"origin":origin,"request":request});
        if let Some(replay) =
            self.manager_v2_replay(&authority.config, &request.idempotency_key, &payload)?
        {
            let mut receipt: ManagerActionReceiptV2 = serde_json::from_value(replay)?;
            receipt.refresh_action_metadata(&request.operation);
            if let Some(id) = receipt.target_session_id {
                if let Some(target) = self.get_session(id)? {
                    if rsi_common::is_container_kind(target.session_kind) {
                        self.manager_v2_require_container(&authority, id, true)?;
                    } else if request.operation.housekeeping_session().is_some()
                        || matches!(request.operation, OperatorCall { .. })
                    {
                        // Reach was re-derived by manager_action_target above.
                    } else {
                        let parent =
                            action_epic(&request.operation).or_else(|| match request.operation {
                                CreateSession { parent_id, .. } => Some(parent_id),
                                _ => None,
                            });
                        if target.project_id != Some(authority.config.project_id)
                            || target.parent_id != parent
                        {
                            return Err(refused("manager_v2_target_out_of_scope"));
                        }
                    }
                }
            }
            return Ok(receipt);
        }
        let admission = self.manager_action_admission(
            &authority,
            &origin,
            &request,
            true,
            true,
            review_source,
        )?;
        let id = admission.target_session_id;
        let operation_id = Uuid::new_v4();
        let receipt = ManagerActionReceiptV2 {
            operation_id,
            state: ManagerActionStateV2::Queued,
            action_kind: request.operation.action_kind(),
            target_type: request.operation.target_type(),
            row_version: 1,
            target_session_id: id,
            outcome: None,
            result: None,
            deduplicated: false,
            operator_result: None,
        };
        let stamp = now();
        let due = (Utc::now() + chrono::Duration::seconds(i64::from(admission.delay_seconds)))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let actor = match origin {
            ManagerActionOriginV2::Agent { caller } => Some(caller),
            ManagerActionOriginV2::OperatingIntent { .. } => None,
        };
        self.conn.execute("INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,scope_version,policy_version,actor_session_id,idempotency_key,fingerprint,kind,payload_json,state,row_version,target_session_id,outcome_json,not_before,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'queued',1,?11,?12,?13,?14,?14)", params![operation_id.to_string(),authority.config.project_id.to_string(),authority.config.manager_session_id.to_string(),authority.config.row_version,authority.grant.row_version,actor.map(|v|v.to_string()),request.idempotency_key,fingerprint(&payload)?,ACTION_KIND,serde_json::to_string(&payload)?,id.map(|v|v.to_string()),serde_json::to_string(&receipt)?,due,stamp])?;
        // Publish manager pause intent at admission, before waiting on an
        // earlier action or a process guard. Automatic effects re-read it.
        if let PauseLead {
            epic_id,
            expected,
            reason,
        } = &request.operation
        {
            self.manager_v2_set_lead_pause(
                &authority.config,
                *epic_id,
                operation_id,
                actor,
                expected.lead_session_id,
                reason,
            )?;
        }
        let manager_pause_version = action_epic(&request.operation)
            .map(|epic| {
                self.manager_v2_lead_pause(&authority.config, epic)
                    .map(|p| p.0)
            })
            .transpose()?
            .unwrap_or(0);
        let context = ManagerActionContextV2 {
            origin,
            request,
            target_session_id: id,
            source: admission.source,
            launch: admission.launch,
            manager_pause_version,
        };
        self.manager_v2_put_record(
            &authority.config,
            CONTEXT_KIND,
            &operation_id.to_string(),
            action_epic(&context.request.operation),
            0,
            &serde_json::to_value(context)?,
        )?;
        self.manager_v2_event(
            &authority.config,
            actor,
            "action_queued",
            &operation_id.to_string(),
            1,
            &serde_json::to_value(&receipt)?,
        )?;
        Ok(receipt)
    }

    pub(crate) fn manager_action_operation(
        &self,
        id: Uuid,
    ) -> Result<Option<ManagerActionOperationV2>> {
        let raw:Option<(String,String,i64,String,String,Option<String>,Option<String>)>=self.conn.query_row(
            "SELECT o.project_id,o.manager_session_id,o.scope_version,o.outcome_json,c.payload_json,o.claim_boot_id,e.payload_json
             FROM harness_manager_v2_operations o JOIN harness_manager_v2_records c ON c.project_id=o.project_id AND c.manager_session_id=o.manager_session_id AND c.scope_version=o.scope_version AND c.kind=?2 AND c.record_key=o.id
             LEFT JOIN harness_manager_v2_records e ON e.project_id=o.project_id AND e.manager_session_id=o.manager_session_id AND e.scope_version=o.scope_version AND e.kind=?3 AND e.record_key=o.id
             WHERE o.id=?1 AND o.kind=?4",params![id.to_string(),CONTEXT_KIND,EXECUTION_KIND,ACTION_KIND],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
        raw.map(
            |(project, manager, scope, receipt, context, boot, execution)| {
                let execution: Value = execution
                    .map(|s| serde_json::from_str(&s))
                    .transpose()?
                    .unwrap_or(json!({}));
                let context: ManagerActionContextV2 = serde_json::from_str(&context)?;
                let mut receipt: ManagerActionReceiptV2 = serde_json::from_str(&receipt)?;
                receipt.refresh_action_metadata(&context.request.operation);
                Ok(ManagerActionOperationV2 {
                    project_id: parse_id(&project)?,
                    manager_session_id: parse_id(&manager)?,
                    scope_version: scope,
                    context,
                    receipt,
                    claim_boot_id: boot.map(|s| parse_id(&s)).transpose()?,
                    effect_started: execution["effect_started"].as_bool().unwrap_or(false),
                    settled_fence: execution
                        .get("settled_fence")
                        .filter(|v| !v.is_null())
                        .map(|v| serde_json::from_value(v.clone()))
                        .transpose()?,
                })
            },
        )
        .transpose()
    }

    pub(crate) fn claim_manager_action(
        &self,
        boot_id: Uuid,
    ) -> Result<Option<ManagerActionClaimV2>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // One active command per project, even across independent daemon handles.
        // An uncertain command fences only conflicting targets, not all future work.
        let id:Option<String>=self.conn.query_row(
            &format!("SELECT o.id FROM harness_manager_v2_operations o WHERE o.kind=?1 AND o.state='queued' AND o.not_before<=?2
             AND NOT EXISTS(SELECT 1 FROM harness_manager_v2_operations a WHERE a.project_id=o.project_id AND a.kind=?1
                AND a.id IS NOT json_extract(o.payload_json,'$.request.operation.operation_id') AND (a.state='running' OR (a.state='uncertain' AND (a.target_session_id=o.target_session_id OR (json_extract(a.payload_json,'$.request.operation.epic_id')=json_extract(o.payload_json,'$.request.operation.epic_id') AND {UNCERTAINTY_CURRENT_AUTHORITY})))))
             AND NOT EXISTS(SELECT 1 FROM manager_root_successions r JOIN sessions p ON p.id=r.predecessor_session_id
                WHERE r.operation_id=o.id AND r.state='reserved' AND p.status IN ('Starting','Running','WaitingApproval')
                  AND r.authority_epoch=(SELECT epoch FROM manager_authority_epochs WHERE project_id=r.project_id)
                  AND EXISTS(SELECT 1 FROM harness_manager_scopes h WHERE h.project_id=r.project_id AND h.row_version=r.scope_version))
             ORDER BY o.not_before,o.id LIMIT 1"),params![ACTION_KIND,now()],|r|r.get(0)).optional()?;
        let Some(id) = id else {
            return Ok(None);
        };
        let id = parse_id(&id)?;
        let mut operation = self
            .manager_action_operation(id)?
            .ok_or_else(|| refused("manager_v2_action_unavailable"))?;
        operation.receipt.state = ManagerActionStateV2::Running;
        operation.receipt.outcome = None;
        operation.receipt.row_version += 1;
        operation
            .receipt
            .refresh_action_metadata(&operation.context.request.operation);
        self.conn.execute("UPDATE harness_manager_v2_operations SET state='running',row_version=?2,outcome_json=?3,attempts=attempts+1,claim_boot_id=?4,updated_at=?5 WHERE id=?1 AND state='queued'",params![id.to_string(),operation.receipt.row_version,serde_json::to_string(&operation.receipt)?,boot_id.to_string(),now()])?;
        operation.claim_boot_id = Some(boot_id);
        self.claim_manager_succession_on(id, boot_id)?;
        tx.commit()?;
        Ok(Some(ManagerActionClaimV2 { operation, boot_id }))
    }

    pub(crate) fn recover_manager_actions_startup(&self, boot_id: Uuid) -> Result<usize> {
        self.recover_manager_action_claims(Some(boot_id))
    }

    /// Caller must own the runtime reconciliation single-flight guard. No
    /// previous execution future can then own a running claim, including an
    /// aborted future from this same boot. Never retry its provider effect.
    pub(crate) fn recover_abandoned_manager_action_claims(&self) -> Result<usize> {
        self.recover_manager_action_claims(None)
    }

    fn recover_manager_action_claims(&self, boot_id: Option<Uuid>) -> Result<usize> {
        let ids: Vec<String> = {
            let mut s=self.conn.prepare("SELECT id FROM harness_manager_v2_operations WHERE kind=?1 AND state='running' AND (?2 IS NULL OR claim_boot_id IS NOT ?2) ORDER BY id LIMIT 64")?;
            s.query_map(
                params![ACTION_KIND, boot_id.map(|id| id.to_string())],
                |r| r.get(0),
            )?
            .collect::<std::result::Result<_, _>>()?
        };
        let mut count = 0;
        for id in ids {
            let Some(operation) = self.manager_action_operation(parse_id(&id)?)? else {
                continue;
            };
            let old_boot = operation
                .claim_boot_id
                .ok_or_else(|| refused("manager_v2_claim_corrupt"))?;
            let claim = ManagerActionClaimV2 {
                operation,
                boot_id: old_boot,
            };
            self.finish_manager_action(
                &claim,
                ManagerActionStateV2::Uncertain,
                "execution_owner_lost_unconfirmed",
            )?;
            count += 1;
        }
        Ok(count)
    }

    pub(super) fn manager_action_assert_claim(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<ManagerActionOperationV2> {
        let op = self
            .manager_action_operation(claim.id())?
            .ok_or_else(|| refused("manager_v2_action_unavailable"))?;
        if op.receipt.state != ManagerActionStateV2::Running
            || op.receipt.row_version != claim.operation.receipt.row_version
            || op.claim_boot_id != Some(claim.boot_id)
        {
            return Err(refused("manager_v2_claim_changed"));
        }
        Ok(op)
    }

    /// Check at the actual spawn/effect boundary. A preflight result is never a
    /// portable capability; every effect must recheck the persisted claim.
    pub(crate) fn manager_action_runtime_gate(
        &self,
        claim: &ManagerActionClaimV2,
        effect: bool,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        if effect {
            let op = self.manager_action_assert_claim(claim)?;
            let authority =
                self.manager_action_authority(&op.context.origin, &op.context.request)?;
            let key = claim.id().to_string();
            let prior = self.manager_v2_record(&authority.config, EXECUTION_KIND, &key)?;
            let mut payload = prior
                .as_ref()
                .map(|r| r.payload.clone())
                .unwrap_or(json!({}));
            payload["effect_started"] = json!(true);
            self.manager_v2_put_record(
                &authority.config,
                EXECUTION_KIND,
                &key,
                action_epic(claim.action()),
                prior.map_or(0, |r| r.row_version),
                &payload,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn manager_action_runtime_gate_on(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<ManagerAuthorityV2> {
        let op = self.manager_action_assert_claim(claim)?;
        let authority = self.manager_action_authority(&op.context.origin, &op.context.request)?;
        // An appointment rotation cannot make a stored daemon intent act under a
        // newer principal even if its numeric scope/policy versions coincide.
        if authority.config.project_id != op.project_id
            || authority.config.manager_session_id != op.manager_session_id
            || authority.config.row_version != op.scope_version
        {
            return Err(refused("manager_v2_scope_changed"));
        }
        let mut action = op.context.request.operation.clone();
        let review_evidence_action = self.manager_review_evidence_action(&op)?;
        if let Some(settled) = op.settled_fence {
            match &mut action {
                ManagerActionV2::PauseLead { expected, .. }
                | ManagerActionV2::ReplaceLead { expected, .. }
                | ManagerActionV2::RetryLead { expected, .. }
                | ManagerActionV2::AssignLead { expected, .. } => *expected = settled,
                _ => return Err(refused("manager_v2_invalid_settlement_witness")),
            }
        }
        let target = self.manager_action_target(&authority, &action, true)?;
        // Retry admission is re-proved at every execution gate: a lead that an
        // operator resumed after admission must not be settled by the retry.
        if let (ManagerActionV2::RetryLead { .. }, Some(lead)) = (&action, target.as_ref()) {
            manager_retry_admissible(lead)?;
        }
        let effect_epic = match action
            .housekeeping_session()
            .or_else(|| operator_delegation::delegated_effect_session(&action))
        {
            // A leaf's nearest Epic owns its decision, pause and hold gates.
            Some(session) => self.manager_action_session_epic(session)?,
            None => action_epic(&action).or_else(|| {
                target
                    .as_ref()
                    .filter(|s| s.session_kind == SessionKind::Epic)
                    .map(|s| s.id)
            }),
        };
        // Check live decisions here, not only the coordinator's last hold
        // projection. Operator answer delivery has a separate continuation
        // intent and does not call this action gate on its own pending answer.
        if !matches!(action, ManagerActionV2::PauseLead { .. }) {
            if let Some(epic) = effect_epic {
                if !review_evidence_action {
                    self.manager_v2_decision_gate(&authority.config, epic)?;
                }
                let (version, paused) = self.manager_v2_lead_pause(&authority.config, epic)?;
                let explicit_resume =
                    matches!(op.context.origin, ManagerActionOriginV2::Agent { .. })
                        && matches!(
                            action,
                            ManagerActionV2::ResumeLead { .. }
                                | ManagerActionV2::RetryLead { .. }
                                | ManagerActionV2::ReplaceLead { .. }
                        );
                if paused
                    && (!explicit_resume || version != op.context.manager_pause_version)
                    && !matches!(
                        action,
                        ManagerActionV2::AssignLead { .. }
                            | ManagerActionV2::RetireLeadContinuations { .. }
                    )
                {
                    return Err(refused("manager_v2_manager_paused"));
                }
                if matches!(
                    op.context.origin,
                    ManagerActionOriginV2::OperatingIntent { .. }
                ) {
                    self.manager_v2_intent_work_gate(&authority.config, epic)?;
                }
            }
        }
        for reason in [
            ManagerActionHoldReasonV2::Resources,
            ManagerActionHoldReasonV2::Decision,
        ] {
            if matches!(action, ManagerActionV2::PauseLead { .. })
                || review_evidence_action && matches!(reason, ManagerActionHoldReasonV2::Decision)
                || matches!(reason, ManagerActionHoldReasonV2::Resources)
                    && op.context.launch.is_none()
            {
                continue;
            }
            for epic in [None, effect_epic] {
                let key = manager_action_hold_key(reason, epic);
                if let Some(record) =
                    self.manager_v2_record(&authority.config, MANAGER_ACTION_HOLD_KIND, &key)?
                {
                    let hold: ManagerActionRuntimeHoldV2 =
                        serde_json::from_value(record.payload)
                            .map_err(|_| refused("manager_v2_runtime_hold_malformed"))?;
                    if hold.blocked {
                        return Err(refused(match reason {
                            ManagerActionHoldReasonV2::Resources => "manager_v2_resource_hold",
                            ManagerActionHoldReasonV2::Decision => "manager_v2_decision_hold",
                        }));
                    }
                }
            }
        }

        if !matches!(action, ManagerActionV2::PauseLead { .. }) {
            if authority.grant.policy.paused
                || effect_epic.is_some_and(|e| authority.grant.policy.paused_epic_ids.contains(&e))
            {
                return Err(refused("manager_v2_policy_paused"));
            }
            // Metadata edits neither answer nor cancel a gate; archive and
            // restore keep the full human/recovery-owner gate.
            if let Some(target) = target.as_ref().filter(|s| {
                rsi_common::is_leaf_kind(s.session_kind)
                    && !matches!(action, ManagerActionV2::UpdateSession { .. })
            }) {
                let allow_interrupted_resume =
                    matches!(op.context.origin, ManagerActionOriginV2::Agent { .. })
                        && matches!(action, ManagerActionV2::ResumeLead { .. })
                        && target.status == SessionStatus::Interrupted;
                if matches!(action, ManagerActionV2::RetireLeadContinuations { .. }) {
                    // Retirement supersedes continuation owners and program
                    // declarations only; operator ownership still refuses.
                    self.manager_action_operator_gate(target.id)?;
                } else {
                    self.manager_action_human_gate_with_interrupted_resume(
                        target.id,
                        allow_interrupted_resume,
                    )?;
                }
            }
        } else if let Some(target) = target.as_ref() {
            // Pausing may stop a running turn, but never answers or cancels a
            // pending approval/question or takes an independent recovery owner.
            self.manager_action_human_gate(target.id)?;
        }
        if let Some(choice) = &op.context.launch {
            let exclude = if matches!(
                action,
                ManagerActionV2::ReplaceLead { .. } | ManagerActionV2::RetryLead { .. }
            ) && op
                .context
                .target_session_id
                .map(|id| self.get_session(id))
                .transpose()?
                .flatten()
                .is_none()
            {
                action_fence(&action).and_then(|f| f.lead_session_id)
            } else {
                op.context.target_session_id
            };
            self.manager_action_resources(&authority, choice, exclude)?;
            if matches!(action, ManagerActionV2::ResumeLead { .. }) {
                let target = target.ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
                if target.provider != choice.provider
                    || target.model.as_deref() != Some(choice.model.as_str())
                    || target.effort != choice.effort
                {
                    return Err(refused("manager_v2_launch_changed"));
                }
            }
        }
        Ok(authority)
    }

    /// Called only with the predecessor spawn guard held, after the runtime has
    /// proved its observed process incarnation settled. Only the event component
    /// may advance; hierarchy and custody authority may never be refreshed away.
    pub(crate) fn manager_action_accept_settled_fence(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let op = self.manager_action_assert_claim(claim)?;
        let authority = self.manager_action_authority(&op.context.origin, &op.context.request)?;
        let epic = action_epic(claim.action())
            .ok_or_else(|| refused("manager_v2_invalid_settlement_witness"))?;
        self.manager_v2_require_epic(&authority, epic)?;
        let old = action_fence(claim.action()).expect("lead fence");
        let current = self.manager_action_lead_fence(epic)?;
        if old.lead_session_id != current.lead_session_id
            || old.lead_generation != current.lead_generation
            || old.custody_generation != current.custody_generation
        {
            return Err(refused("manager_v2_lead_changed"));
        }
        if let Some(id) = current.lead_session_id {
            let session = self
                .get_session(id)?
                .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
            if matches!(
                session.status,
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
            ) {
                return Err(refused("manager_v2_predecessor_unsettled"));
            }
        }
        let key = claim.id().to_string();
        let prior = self.manager_v2_record(&authority.config, EXECUTION_KIND, &key)?;
        let mut payload = prior
            .as_ref()
            .map(|r| r.payload.clone())
            .unwrap_or(json!({}));
        payload["settled_fence"] = serde_json::to_value(current)?;
        self.manager_v2_put_record(
            &authority.config,
            EXECUTION_KIND,
            &key,
            Some(epic),
            prior.map_or(0, |r| r.row_version),
            &payload,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A manager continuation publishes its admitted invocation before the
    /// backend effect, rather than relying on the generic asynchronous mirror.
    pub(crate) fn bind_manager_resume_invocation(
        &self,
        claim: &ManagerActionClaimV2,
        invocation: Uuid,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        if !matches!(claim.action(), ManagerActionV2::ResumeLead { .. }) {
            return Err(refused("manager_v2_not_resume_action"));
        }
        let target = claim
            .operation
            .context
            .target_session_id
            .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
        let bound:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM model_invocations WHERE id=?1 AND session_id=?2 AND project_id=?3 AND dedup_key=?4)",params![invocation.to_string(),target.to_string(),claim.operation.project_id.to_string(),format!("manager.action:{}",claim.id())],|r|r.get(0))?;
        if !bound {
            return Err(refused("manager_v2_invocation_changed"));
        }
        self.set_session_model_invocation(target, Some(invocation))?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn finish_manager_action(
        &self,
        claim: &ManagerActionClaimV2,
        state: ManagerActionStateV2,
        outcome: &str,
    ) -> Result<ManagerActionReceiptV2> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let receipt = self.finish_manager_action_on(claim, state, outcome)?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(super) fn finish_manager_action_on(
        &self,
        claim: &ManagerActionClaimV2,
        state: ManagerActionStateV2,
        outcome: &str,
    ) -> Result<ManagerActionReceiptV2> {
        self.finish_manager_action_result_on(claim, state, outcome, None)
    }

    /// `finish_manager_action_on` plus the bounded K14 `operator_result`.
    pub(super) fn finish_manager_action_result_on(
        &self,
        claim: &ManagerActionClaimV2,
        state: ManagerActionStateV2,
        outcome: &str,
        operator_result: Option<rsi_common::manager_operator_delegation::OperatorCallResultV1>,
    ) -> Result<ManagerActionReceiptV2> {
        if let Some(result) = &operator_result {
            result.validate().map_err(refused)?;
        }
        if matches!(
            state,
            ManagerActionStateV2::Queued | ManagerActionStateV2::Running
        ) {
            return Err(refused("manager_v2_invalid_result"));
        }
        text(outcome, 256).map_err(refused)?;
        let op = self.manager_action_assert_claim(claim)?;
        let state = self.finish_manager_succession_on(claim.id(), state)?;
        let mut receipt = op.receipt;
        receipt.state = state;
        receipt.row_version += 1;
        receipt.outcome = Some(outcome.into());
        receipt.refresh_action_metadata(&op.context.request.operation);
        receipt.operator_result = operator_result.map(Box::new);
        let changed=self.conn.execute("UPDATE harness_manager_v2_operations SET state=?2,row_version=?3,outcome_json=?4,updated_at=?5 WHERE id=?1 AND state='running' AND row_version=?6 AND claim_boot_id=?7",params![claim.id().to_string(),state_name(state),receipt.row_version,serde_json::to_string(&receipt)?,now(),claim.operation.receipt.row_version,claim.boot_id.to_string()])?;
        if changed != 1 {
            return Err(refused("manager_v2_claim_changed"));
        }
        if state == ManagerActionStateV2::Succeeded
            && matches!(op.context.origin, ManagerActionOriginV2::Agent { .. })
            && matches!(
                op.context.request.operation,
                ManagerActionV2::ResumeLead { .. }
                    | ManagerActionV2::RetryLead { .. }
                    | ManagerActionV2::ReplaceLead { .. }
            )
        {
            if let Some(config) = self.get_harness_manager(op.project_id)? {
                if config.manager_session_id == op.manager_session_id
                    && config.row_version == op.scope_version
                {
                    let epic = action_epic(&op.context.request.operation).expect("lead action");
                    self.manager_v2_clear_lead_pause(
                        &config,
                        epic,
                        op.context.manager_pause_version,
                        &format!("manager_action:{}", claim.id()),
                    )?;
                }
            }
        }
        let actor = match op.context.origin {
            ManagerActionOriginV2::Agent { caller } => Some(caller.to_string()),
            ManagerActionOriginV2::OperatingIntent { .. } => None,
        };
        self.conn.execute("INSERT INTO harness_manager_v2_events(project_id,manager_session_id,scope_version,actor_session_id,kind,record_key,row_version,payload_json,created_at) VALUES(?1,?2,?3,?4,'action_result',?5,?6,?7,?8)",params![op.project_id.to_string(),op.manager_session_id.to_string(),op.scope_version,actor,claim.id().to_string(),receipt.row_version,serde_json::to_string(&receipt)?,now()])?;
        self.conn.execute(
            "UPDATE harness_manager_v2_records SET archived=1,updated_at=?5
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
               AND record_key=?4 AND kind IN ('lifecycle_context','lifecycle_execution')",
            params![
                op.project_id.to_string(),
                op.manager_session_id.to_string(),
                op.scope_version,
                claim.id().to_string(),
                now()
            ],
        )?;
        Ok(receipt)
    }

    /// One transaction owns container identity, normalized tags, provenance,
    /// effective v1 enrollment, result and audit. Never call cascading operators.
    pub(crate) fn apply_manager_container_action(
        &self,
        claim: &ManagerActionClaimV2,
        created: Option<&Session>,
    ) -> Result<(Session, Vec<Uuid>)> {
        use ManagerActionV2::*;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = self.manager_action_runtime_gate_on(claim)?;
        let id = claim
            .operation
            .context
            .target_session_id
            .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
        let mut transitioned = vec![id];
        match claim.action() {
            CreateContainer {
                kind, parent_id, ..
            } => {
                let session = created.ok_or_else(|| refused("manager_v2_container_unavailable"))?;
                if session.id != id
                    || session.parent_id != *parent_id
                    || session.session_kind != *kind
                    || session.project_id != Some(authority.config.project_id)
                    || session.lead_session_id.is_some()
                    || session.sandbox_root.is_some()
                {
                    return Err(refused("manager_v2_container_identity_changed"));
                }
                // Pending reservations have already been counted at admission.
                if *kind == SessionKind::Epic
                    && !authority.config.has_dynamic_scope()
                    && authority.config.epic_ids.len() >= 32
                {
                    return Err(refused("manager_v2_scope_limit"));
                }
                self.insert_session(session)?;
                for tag in &session.tags {
                    self.conn.execute(
                        "INSERT INTO session_tags(session_id,tag) VALUES(?1,?2)",
                        params![id.to_string(), tag],
                    )?;
                }
                self.manager_action_bind_entity(
                    claim,
                    if *kind == SessionKind::Epic {
                        "Epic"
                    } else {
                        "Group"
                    },
                )?;
            }
            UpdateContainer {
                name, description, ..
            } => {
                self.conn.execute("UPDATE sessions SET title=?2,query=?2,description=?3,updated_at=?4 WHERE id=?1",params![id.to_string(),name,description,now()])?;
            }
            ArchiveContainer { .. } => {
                transitioned = self.archive_manager_container_rows(id)?;
            }
            DeleteContainer { .. } => {
                self.conn.execute("UPDATE sessions SET status='Deleted',lead_session_id=NULL,updated_at=?2 WHERE id=?1", params![id.to_string(), now()])?;
            }
            RestoreContainer { .. } => {
                transitioned = self.restore_manager_container_rows(id)?;
            }
            _ => return Err(refused("manager_v2_not_container_action")),
        }
        self.finish_manager_action_on(
            claim,
            ManagerActionStateV2::Succeeded,
            "container_committed",
        )?;
        if matches!(claim.action(), ManagerActionV2::ArchiveContainer { .. }) {
            let archived_ids = transitioned
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            self.conn.execute(
                "UPDATE harness_manager_v2_operations SET outcome_json=json_set(outcome_json,'$.execution.cascade_archived_ids',json(?2)) WHERE id=?1",
                params![claim.id().to_string(), serde_json::to_string(&archived_ids)?],
            )?;
        }
        let session = self
            .get_session(id)?
            .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
        tx.commit()?;
        Ok((session, transitioned))
    }

    /// One transaction re-runs the runtime gate (scope, grant, fence, state,
    /// human gate), applies the single-row effect and records the result.
    /// Archive never runs archive cleanup or a sandbox purge.
    pub(crate) fn apply_manager_session_action(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<Session> {
        use ManagerActionV2::{ArchiveSession, RestoreSession, UpdateSession};
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = self.manager_action_runtime_gate_on(claim)?;
        let id = claim
            .action()
            .housekeeping_session()
            .ok_or_else(|| refused("manager_v2_not_session_action"))?;
        let stamp = now();
        let outcome = match claim.action() {
            ArchiveSession { .. } => {
                let changed = self.conn.execute(
                    "UPDATE sessions SET status='Archived',pending_archive=0,updated_at=?2
                     WHERE id=?1 AND status IN ('Completed','Failed','Interrupted')",
                    params![id.to_string(), stamp],
                )?;
                if changed != 1 {
                    return Err(refused("manager_v2_session_state_changed"));
                }
                "session_archived"
            }
            RestoreSession { .. } => {
                let changed = self.conn.execute(
                    "UPDATE sessions SET status='Completed',pending_archive=0,updated_at=?2
                     WHERE id=?1 AND status='Archived'",
                    params![id.to_string(), stamp],
                )?;
                if changed != 1 {
                    return Err(refused("manager_v2_session_state_changed"));
                }
                "session_restored"
            }
            UpdateSession { patch, .. } => {
                let tags = self.manager_action_session_patch_tags(&authority, patch)?;
                let key = id.to_string();
                if let Some(title) = &patch.title {
                    self.conn.execute(
                        "UPDATE sessions SET title=?2 WHERE id=?1",
                        params![key, title],
                    )?;
                }
                if let Some(description) = &patch.description {
                    self.conn.execute(
                        "UPDATE sessions SET description=?2 WHERE id=?1",
                        params![key, description.value()],
                    )?;
                }
                if let Some(rating) = &patch.rating {
                    self.conn.execute(
                        "UPDATE sessions SET rating=?2 WHERE id=?1",
                        params![key, rating.value().map(|v| i64::from(*v))],
                    )?;
                }
                if let Some(task) = &patch.active_task {
                    self.conn.execute(
                        "UPDATE sessions SET active_task=?2 WHERE id=?1",
                        params![key, task.value()],
                    )?;
                }
                if let Some(label) = &patch.label {
                    self.conn.execute(
                        "UPDATE sessions SET group_id=?2 WHERE id=?1",
                        params![key, label.value().map(ToString::to_string)],
                    )?;
                }
                if let Some(tags) = tags {
                    self.conn
                        .execute("DELETE FROM session_tags WHERE session_id=?1", [&key])?;
                    for tag in &tags {
                        self.conn.execute(
                            "INSERT INTO session_tags(session_id,tag) VALUES(?1,?2)",
                            params![key, tag],
                        )?;
                    }
                    // Same legacy `sessions.tag` projection as tag_ops: the
                    // lexicographically first tag (the set is non-empty).
                    self.conn.execute(
                        "UPDATE sessions SET tag=(SELECT tag FROM session_tags
                           WHERE session_id=?1 ORDER BY tag ASC LIMIT 1) WHERE id=?1",
                        [&key],
                    )?;
                }
                self.conn.execute(
                    "UPDATE sessions SET updated_at=?2 WHERE id=?1",
                    params![key, stamp],
                )?;
                "session_updated"
            }
            _ => return Err(refused("manager_v2_not_session_action")),
        };
        self.finish_manager_action_on(claim, ManagerActionStateV2::Succeeded, outcome)?;
        let session = self
            .get_session(id)?
            .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
        tx.commit()?;
        Ok(session)
    }

    /// Archive the validated cascade rows without touching already-Archived rows.
    fn archive_manager_container_rows(&self, id: Uuid) -> Result<Vec<Uuid>> {
        let members = self.manager_action_validate_archive_cascade(id)?;
        let transitioned = members
            .iter()
            .filter(|(_, was_archived)| !was_archived)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let stamp = now();
        for (member, was_archived) in &members {
            if !*was_archived {
                self.conn.execute(
                    "UPDATE sessions SET status='Archived',lead_session_id=NULL,updated_at=?2 WHERE id=?1 AND status IN ('Completed','Failed','Interrupted')",
                    params![member.to_string(), stamp],
                )?;
            }
        }
        Ok(transitioned)
    }

    /// Apply restore gates and row transitions inside the caller's transaction.
    fn restore_manager_container_rows(&self, id: Uuid) -> Result<Vec<Uuid>> {
        let container = self
            .get_session(id)?
            .ok_or_else(|| refused("manager_v2_container_changed"))?;
        let recorded = self.manager_action_validate_restore_container(&container, true)?;
        let mut members = recorded
            .as_ref()
            .map(|record| record.ids.clone())
            .unwrap_or_default();
        if recorded.is_some() && !members.contains(&id) {
            members.push(id);
        }
        // A Deleted container restore retains the original manager behavior.
        // Cascade evidence applies only to the exact Archived rows it recorded.
        if recorded.is_none() {
            members = vec![id];
        }
        let stamp = now();
        let mut transitioned = Vec::new();
        for member in members {
            let status: Option<String> = self
                .conn
                .query_row(
                    "SELECT status FROM sessions WHERE id=?1",
                    [member.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if status.as_deref() == Some("Archived")
                || (member == id && recorded.is_none() && status.as_deref() == Some("Deleted"))
            {
                let previous_status = status.as_deref().unwrap_or("Archived");
                let changed = self.conn.execute(
                    "UPDATE sessions SET status='Completed',pending_archive=0,lead_session_id=NULL,updated_at=?2 WHERE id=?1 AND status=?3",
                    params![member.to_string(), stamp, previous_status],
                )?;
                if changed == 1 {
                    transitioned.push(member);
                }
            }
        }
        Ok(transitioned)
    }

    pub(crate) fn manager_action_bind_entity(
        &self,
        claim: &ManagerActionClaimV2,
        kind: &str,
    ) -> Result<()> {
        self.conn.execute("INSERT INTO harness_manager_v2_entities(session_id,operation_id,project_id,manager_session_id,scope_version,policy_version,kind,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![claim.operation.context.target_session_id.map(|v|v.to_string()),claim.id().to_string(),claim.operation.project_id.to_string(),claim.operation.manager_session_id.to_string(),claim.operation.scope_version,claim.operation.context.request.fence.policy_version,kind,now()])?;
        Ok(())
    }

    /// The runtime proves provider establishment and quiescent old ownership
    /// while holding both spawn guards and active-map custody before this CAS.
    pub(crate) fn commit_manager_lead_action(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<Vec<Uuid>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        let epic =
            action_epic(claim.action()).ok_or_else(|| refused("manager_v2_epic_unavailable"))?;
        let target = claim.operation.context.target_session_id;
        if let Some(target) = target {
            let candidate = self
                .get_session(target)?
                .ok_or_else(|| refused("manager_v2_candidate_unavailable"))?;
            if candidate.parent_id != Some(epic)
                || candidate.project_id != Some(claim.operation.project_id)
                || !rsi_common::legal_children(Some(SessionKind::Epic))
                    .contains(&candidate.session_kind)
                || !rsi_common::is_leaf_kind(candidate.session_kind)
                || !matches!(
                    candidate.status,
                    SessionStatus::Running
                        | SessionStatus::Starting
                        | SessionStatus::Completed
                        | SessionStatus::Interrupted
                        | SessionStatus::Failed
                )
            {
                return Err(refused("manager_v2_candidate_changed"));
            }
        }
        let expected = action_fence(claim.action()).expect("lead fence");
        self.reject_nonterminal_agent_successor_epic_lead_mutation(epic)?;
        let changed=self.conn.execute("UPDATE sessions SET lead_session_id=?2,updated_at=?3 WHERE id=?1 AND lead_session_id IS ?4 AND EXISTS(SELECT 1 FROM epic_lead_generations WHERE epic_id=?1 AND generation=?5)",params![epic.to_string(),target.map(|v|v.to_string()),now(),expected.lead_session_id.map(|v|v.to_string()),expected.lead_generation])?;
        if changed != 1 {
            return Err(refused("manager_v2_lead_changed"));
        }
        let jobs = match (expected.lead_session_id, target) {
            (Some(old), Some(new)) if old != new => {
                self.readdress_open_manager_requests(epic, old, new)?
            }
            _ => Vec::new(),
        };
        if matches!(
            claim.action(),
            ManagerActionV2::RetryLead { .. } | ManagerActionV2::ReplaceLead { .. }
        ) {
            self.manager_action_bind_entity(claim, "session")?;
        }
        self.finish_manager_action_on(claim, ManagerActionStateV2::Succeeded, "lead_committed")?;
        tx.commit()?;
        Ok(jobs)
    }

    pub(crate) fn commit_manager_created_session(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        self.manager_action_bind_entity(claim, "session")?;
        self.finish_manager_action_on(
            claim,
            ManagerActionStateV2::Succeeded,
            "session_established",
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn parse_id(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| refused("manager_v2_invalid_stored_identity"))
}
