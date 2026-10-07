//! Durable, attributed manager lifecycle commands. V104 is the only storage
//! surface: admission reserves identity, execution claims before effects, and
//! lost execution ownership becomes uncertain rather than being replayed.

use super::Store;
use super::harness_manager_v2::{ManagerAuthorityV2, fingerprint, now, refused};
use super::sessions::historical_session_restore_blocked_on;
use crate::error::Result;
use chrono::Utc;
use rsi_common::harness_manager_v2::*;
use rsi_common::manager_operator_delegation::DelegatedOperatorCallV1;
use rsi_common::types::{SandboxCleanupState, SandboxKind, Session, SessionKind, SessionStatus};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;

pub mod fence;
mod issue_worker;
mod operator_delegation;
mod recovery;
mod sandbox_source;
mod superseded_wakes;

const ACTION_KIND: &str = "lifecycle_action";
/// #1118: independent `create_session` launches in one project may be
/// `running` at once up to this many; every other action keeps the
/// one-active-command-per-project fence.
pub const MANAGER_CREATE_PARALLELISM: i64 = 3;
/// #1143: receipt outcome (state `uncertain`) of a launch whose deadline
/// could not prove the candidate's process and task exited.
pub const MANAGER_LAUNCH_DRAIN_PENDING: &str = "manager_v2_launch_drain_pending";
/// #1143: the same receipt once the sweep proved the drain and settled the rows.
pub const MANAGER_LAUNCH_DRAIN_VERIFIED: &str = "manager_v2_launch_drain_verified";
const CONTEXT_KIND: &str = "lifecycle_context";
const EXECUTION_KIND: &str = "lifecycle_execution";
const MAX_PENDING: i64 = 64;
/// Durable witness that a terminal/idle lead's exact agent-declared program
/// outcome was superseded by an explicit manager recovery (K2, #390).
pub const LEAD_RETIREMENT_KIND: &str = "lead_continuation_retirement";
pub const RETIREMENT_PAUSE_REASON: &str = "retire_lead_continuations";
/// Settlement outcome codes. The original receipt and evidence are retained;
/// settlement is appended and never re-executes an effect.
pub const SETTLED_EFFECT_ABSENT: &str = "manager_v2_recovered_effect_absent";
pub const SETTLED_PAUSE_CONFIRMED: &str = "lead_paused_reconciled";
const UNCERTAIN_RECOVERY_BATCH: i64 = 16;
const MAX_SETTLE_NESTING: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperatorPause {
    None,
    Soft,
    Hard,
}

/// Read model behind `GetOperatorPause` (#1541).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorPauseDetail {
    pub level: OperatorPause,
    /// RFC3339 time the marker was last written (inherited markers carry the
    /// publication time).
    pub since: Option<String>,
    pub held_succession: bool,
}

impl OperatorPause {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "false",
            Self::Soft => "soft",
            Self::Hard => "hard",
        }
    }
}

/// A lead pause prevents continuations, not inert delegated cleanup or reads.
fn delegated_cleanup_allowed_during_lead_pause(action: &ManagerActionV2) -> bool {
    matches!(
        action,
        ManagerActionV2::OperatorCall { call, .. }
            if matches!(
                call.typed(),
                Ok(DelegatedOperatorCallV1::ArchiveSession(_)
                    | DelegatedOperatorCallV1::UnarchiveSession(_)
                    | DelegatedOperatorCallV1::ListSessions(_)
                    | DelegatedOperatorCallV1::GetArchiveCleanupStatus(_))
            )
    )
}
/// Idempotency-key namespace owned by the DB-native review allocator (#674
/// K15A-1). Only the allocator journals keys in it, so an operation linked by
/// `manager_review_assignments.action_operation_id` is a genuine review launch.
pub(super) const REVIEW_ALLOCATION_KEY_PREFIX: &str = "manager-review-allocation:";
/// Alias `o` is an operation row. A blocked or revoked operation whose target
/// session was never created consumed nothing and is not charged; queued,
/// running, succeeded, failed and uncertain operations stay charged. This
/// includes `succeed_manager` (#1390): a succession refused before its
/// candidate existed releases its charge, so a retried succession does not
/// burn `max_created_sessions`.
const CREATION_CHARGED: &str = "NOT (o.state IN ('blocked','revoked') AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.id=o.target_session_id))";

/// Which caller runs [`Store::recovery_owner_gate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryOwnerMode {
    /// A manager lifecycle action. Byte-identical to the pre-extraction
    /// `manager_action_human_gate_with_interrupted_resume`.
    ManagerAction {
        allow_interrupted_resume: bool,
        allow_soft_operator_pause: bool,
    },
    /// A terminal manager archive leaves an operator interruption marker in
    /// place for a later restore or continuation. It may settle C5 and resume
    /// owners without restarting the session.
    ManagerArchive,
    /// `AgentArchiveChild`: the strictest program mode, with no
    /// interrupted-resume exception, and no hold on the C5 marker that the
    /// archive commit settles itself.
    AgentArchive,
    /// #953: an explicit manager `replace_lead`/`retry_lead` of the current
    /// lead. The outgoing lead's own ordinary resume wakes are superseded (the
    /// lead CAS disables them atomically), so they do not hold the handover.
    /// Every genuine human/operator owner (question, approval, hard operator
    /// pause, C5, capacity incident) and the program gate still refuse, as
    /// for [`Self::ManagerAction`] with a soft-pause exception.
    ManagerLeadHandover,
    /// #1042: a manager `pause_lead`. The lead's own ordinary resume wakes do
    /// not hold the pause: the action suspends them (recorded, restored by
    /// `resume_lead`). Every genuine human/operator owner still refuses.
    ManagerLeadPause,
    /// #1408: admission of a manager's own `succeed_manager` handoff. The
    /// operator pause permits a reservation, which stays queued until cleared.
    /// Effect and publication gates still enforce the operator pause: provider
    /// establishment is productive work, not just a metadata handoff.
    /// Every other human/operator owner still refuses.
    ManagerSuccession,
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
pub const MANAGER_ACTION_HOLD_KIND: &str = "lifecycle_hold";
#[derive(Debug, Clone, Copy)]
pub enum ManagerActionHoldReasonV2 {
    Resources,
    Decision,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerActionRuntimeHoldV2 {
    pub blocked: bool,
}
pub fn manager_action_hold_key(reason: ManagerActionHoldReasonV2, epic: Option<Uuid>) -> String {
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
pub enum ManagerActionOriginV2 {
    Agent { caller: Uuid },
    OperatingIntent { project_id: Uuid, intent_id: Uuid },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerActionSourceV2 {
    pub session_id: Uuid,
    pub working_dir: PathBuf,
    pub sandbox_root: Option<PathBuf>,
    pub commit: String,
    pub custody_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub historical_commit: bool,
    /// #1144: the symbolic ref (`refs/heads/..`) the source HEAD was attached
    /// to when frozen; `None` when detached or for rows frozen before this
    /// field existed. A moved source is accepted only on this same branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// #1195: the explicit or default sandbox source of a `create_session`.
    /// `None` is the pre-#1195 pinned fork of the source's own `HEAD` (lead
    /// replacement, retry and older rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_source: Option<ManagerFrozenSandboxSourceV1>,
}

/// #1195: how a frozen create source's `commit` was chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerFrozenSandboxSourceV1 {
    /// Resolved at launch to the fetched `origin/rolling` tip; `commit` is
    /// the checkout `HEAD` used only when the checkout is not on `rolling`
    /// or no published tip can be observed.
    Rolling,
    /// `commit` is the caller-named commit.
    Commit,
    /// `commit` is the named worktree's `HEAD`, pinned at admission.
    Path {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "is_false")]
        source_dirty: bool,
    },
}

impl ManagerActionSourceV2 {
    /// The launch forks from exactly the frozen `commit` (an explicit or
    /// historical source), not from the source's current `HEAD`.
    #[must_use]
    pub const fn forks_at_frozen_commit(&self) -> bool {
        self.historical_commit || self.sandbox_source.is_some()
    }

    /// The launch resolves the published `rolling` tip (#1195).
    #[must_use]
    pub const fn resolves_rolling_at_launch(&self) -> bool {
        matches!(
            self.sandbox_source,
            Some(ManagerFrozenSandboxSourceV1::Rolling)
        )
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The Issue an `AgentManagerLaunchIssueWorker` action is bound to (#1100).
/// It rides in the action's journalled payload and context, never in the
/// public action wire type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerIssueBindingV1 {
    /// Manager-supplied bounded QA delegation; old bindings default to false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub qa_lane: bool,
    pub issue_id: Uuid,
    pub display_number: i64,
    /// #1254: the terminal worker this launch continues; the launch sets the
    /// new worker's `continued_from` and inherits its display identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_from: Option<Uuid>,
    /// #1590: the implementer Issue (display number) whose text and handoff
    /// the daemon copied into this reviewer's brief. Journalled so a replay
    /// reuses the original brief.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_of: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerActionContextV2 {
    pub origin: ManagerActionOriginV2,
    pub request: AgentManagerControlRequestV2,
    pub target_session_id: Option<Uuid>,
    pub source: Option<ManagerActionSourceV2>,
    pub launch: Option<ManagerLaunchChoiceV2>,
    /// Admission witness: an older explicit resume cannot clear a newer pause.
    #[serde(default)]
    pub manager_pause_version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_binding: Option<ManagerIssueBindingV1>,
}

#[derive(Debug, Clone)]
pub struct ManagerActionOperationV2 {
    pub project_id: Uuid,
    pub manager_session_id: Uuid,
    pub scope_version: i64,
    pub context: ManagerActionContextV2,
    pub receipt: ManagerActionReceiptV2,
    pub effect_started: bool,
    pub settled_fence: Option<ManagerLeadFenceV2>,
    pub claim_boot_id: Option<Uuid>,
}

impl ManagerActionOperationV2 {
    /// Continuations resume work already admitted, even though their journal
    /// uses the same CreateSession action as a new Issue worker.
    pub fn is_new_worker_create(&self) -> bool {
        matches!(
            self.context.request.operation,
            ManagerActionV2::CreateSession { .. }
        ) && self
            .context
            .issue_binding
            .as_ref()
            .and_then(|binding| binding.continue_from)
            .is_none()
    }
}

#[derive(Debug, Clone)]
pub struct ManagerActionClaimV2 {
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
pub struct ManagerCascadeArchiveRecord {
    pub ids: Vec<Uuid>,
    pub completed_at: chrono::DateTime<chrono::FixedOffset>,
}

impl ManagerActionClaimV2 {
    pub fn id(&self) -> Uuid {
        self.operation.receipt.operation_id
    }
    pub fn action(&self) -> &ManagerActionV2 {
        &self.operation.context.request.operation
    }
}

pub fn action_epic(action: &ManagerActionV2) -> Option<Uuid> {
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

pub fn action_fence(action: &ManagerActionV2) -> Option<&ManagerLeadFenceV2> {
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
pub fn manager_resume_available(lead: &Session) -> Result<()> {
    let settled = matches!(
        lead.status,
        SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
    );
    if settled {
        crate::store_support::manager_gates::check_manager_resume_target(lead)?;
    }
    Ok(())
}

/// `retry_lead` admits a `Failed` or `Interrupted` lead, and a `Completed` lead
/// that fails `lifecycle::manager_lead_provider_resumable` (the predicate that
/// resume admission and the continuation gate use). Applied at admission and
/// again at execution.
pub fn manager_retry_admissible(lead: &Session) -> Result<()> {
    match lead.status {
        SessionStatus::Failed | SessionStatus::Interrupted => Ok(()),
        SessionStatus::Completed
            if !crate::store_support::manager_gates::manager_lead_provider_resumable(lead) =>
        {
            Ok(())
        }
        SessionStatus::Completed => Err(
            crate::store_support::manager_gates::manager_refusal_with_next_action(
                "manager_v2_retry_lead_resumable",
                "resume_lead",
            ),
        ),
        _ => Err(refused("manager_v2_retry_requires_terminal")),
    }
}

impl Store {
    /// The latest failed resume invocation carries the transcript refusal.
    /// A later successful attempt clears that evidence even if the provider
    /// thread ID has not changed.
    fn manager_lead_transcript_refusal_class(&self, lead: &Session) -> Result<Option<String>> {
        if lead.provider != rsi_common::types::SessionProvider::Codex {
            return Ok(None);
        }
        let latest: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT status,error_class FROM model_invocations
             WHERE session_id=?1 AND purpose='session.continue.resume'
             ORDER BY created_at DESC,rowid DESC LIMIT 1",
                [lead.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match latest {
            Some((status, Some(class)))
                if status == "failed"
                    && matches!(
                        class.as_str(),
                        "codex_resume_rollout_torn_tail" | "codex_resume_tool_history_invalid"
                    ) =>
            {
                Some(class)
            }
            _ => None,
        })
    }

    pub fn manager_lead_transcript_unresumable(&self, lead: &Session) -> Result<bool> {
        Ok(self.manager_lead_transcript_refusal_class(lead)?.is_some())
    }

    pub(crate) fn manager_lead_torn_tail(&self, lead: &Session) -> Result<bool> {
        Ok(self.manager_lead_transcript_refusal_class(lead)?.as_deref()
            == Some("codex_resume_rollout_torn_tail"))
    }

    pub(crate) fn manager_lead_invalid_tool_history(&self, lead: &Session) -> Result<bool> {
        Ok(self.manager_lead_transcript_refusal_class(lead)?.as_deref()
            == Some("codex_resume_tool_history_invalid"))
    }

    pub fn manager_resume_available_for_lead(&self, lead: &Session) -> Result<()> {
        manager_resume_available(lead)?;
        if matches!(
            lead.status,
            SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
        ) && self.manager_lead_transcript_unresumable(lead)?
        {
            return Err(crate::store_support::manager_gates::manager_resume_unavailable());
        }
        Ok(())
    }

    pub fn manager_retry_admissible_for_lead(&self, lead: &Session) -> Result<()> {
        if lead.status == SessionStatus::Completed
            && self.manager_lead_transcript_unresumable(lead)?
        {
            return Ok(());
        }
        manager_retry_admissible(lead)
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
    /// Archive is a terminal operator decision. Settle queued lead recovery
    /// before it can be claimed; a later manager poll must not relaunch it.
    /// The caller includes this in the same transaction as the Archived row.
    pub(crate) fn cancel_queued_recovery_for_archive_on(
        tx: &rusqlite::Connection,
        session_id: Uuid,
    ) -> Result<()> {
        let key = session_id.to_string();
        let mut stmt = tx.prepare(
            "SELECT id,outcome_json FROM harness_manager_v2_operations
             WHERE kind='lifecycle_action' AND state='queued'
               AND json_extract(payload_json,'$.request.operation.action')
                   IN ('resume_lead','retry_lead','replace_lead')
               AND (json_extract(payload_json,'$.request.operation.epic_id')=?1
                    OR json_extract(payload_json,'$.request.operation.expected.lead_session_id')=?1)
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map([&key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let stamp = now();
        for (id, raw) in rows {
            let mut receipt: ManagerActionReceiptV2 = serde_json::from_str(&raw)?;
            receipt.state = ManagerActionStateV2::Blocked;
            receipt.row_version += 1;
            receipt.outcome = Some("cancelled_by_archive".into());
            tx.execute(
                "UPDATE harness_manager_v2_operations
                 SET state='blocked',row_version=?2,outcome_json=?3,updated_at=?4
                 WHERE id=?1 AND state='queued'",
                params![
                    id,
                    receipt.row_version,
                    serde_json::to_string(&receipt)?,
                    stamp
                ],
            )?;
        }
        tx.execute(
            "UPDATE scheduled_jobs SET enabled=0,updated_at=?2
             WHERE wake_session_id=?1 AND enabled=1 AND wake_mode='resume'",
            params![key, stamp],
        )?;
        Ok(())
    }

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
            // #1235: the journalled request carries its target project, so
            // every effect-time recheck re-resolves the same arm.
            ManagerActionOriginV2::Agent { caller } => self.manager_v2_authorize_in(
                *caller,
                request.project_id,
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

    /// The authority a JOURNALLED action re-proves at every execution gate.
    ///
    /// #1335: an agent's action can wait in the queue (a deploy drain holds a
    /// create for up to an hour) while its seat rotates: the caller is then
    /// an archived predecessor and its own authorization fails with
    /// `manager_current_session_required`. A rotation keeps the appointment
    /// (same anchor, scope and policy versions, which every caller of this
    /// re-checks against the journalled row), so the action is re-proved as
    /// the caller's lineage tip: the seat that holds the caller's authority
    /// now. A caller with no live successor is refused with
    /// `manager_v2_actor_seat_retired`, a clear class the receipt and the
    /// manager notice carry. Admission ([`Self::manager_action_authority`])
    /// never takes this path: only a stored request is inherited.
    pub(super) fn manager_stored_action_authority(
        &self,
        origin: &ManagerActionOriginV2,
        request: &AgentManagerControlRequestV2,
    ) -> Result<ManagerAuthorityV2> {
        let denial = match self.manager_action_authority(origin, request) {
            Ok(authority) => return Ok(authority),
            Err(denial) => denial,
        };
        let ManagerActionOriginV2::Agent { caller } = origin else {
            return Err(denial);
        };
        let caller_retired = self.get_session(*caller)?.is_none_or(|session| {
            matches!(
                session.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
        });
        let tip = self.manager_lineage_tip(*caller).ok();
        match tip.filter(|tip| tip != caller) {
            Some(tip) => self
                .manager_action_authority(&ManagerActionOriginV2::Agent { caller: tip }, request),
            None if caller_retired
                && denial
                    .to_string()
                    .contains("manager_current_session_required") =>
            {
                Err(refused("manager_v2_actor_seat_retired"))
            }
            None => Err(denial),
        }
    }

    /// Read-only exact fence for progress/decision/coordinator observations.
    pub fn manager_action_lead_fence(&self, epic_id: Uuid) -> Result<ManagerLeadFenceV2> {
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
                // #1276 rule (c): assigning a session an ancestor owns as the
                // lead is a mutation of it.
                self.manager_action_refuse_ancestor_owned(authority, candidate.id)?;
            }
            // #1276 rule (c): every lead action mutates the current lead, so it
            // flows down only, at admission and again at effect.
            if let Some(lead) = container.lead_session_id {
                self.manager_action_refuse_ancestor_owned(authority, lead)?;
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
        // #1235 rule (c): housekeeping is a mutation and flows down only.
        self.manager_action_refuse_ancestor_owned(authority, scope.target.id)?;
        Ok((scope.target, scope.epic_id))
    }

    /// #1235 rule (c), mutations flow down: refuse a target owned by a
    /// principal above the acting one.
    fn manager_action_refuse_ancestor_owned(
        &self,
        authority: &ManagerAuthorityV2,
        target: Uuid,
    ) -> Result<()> {
        if self.manager_target_owned_by_ancestor(&authority.config, target)? {
            return Err(refused(
                rsi_common::global_manager::MANAGER_TARGET_OWNED_BY_ANCESTOR,
            ));
        }
        Ok(())
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
    pub(crate) fn manager_action_validate_archive_session(&self, session: &Session) -> Result<()> {
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
        self.manager_action_archive_gate(session.id)
    }

    fn manager_action_live_worktree_blocked(session: &Session) -> bool {
        if session.sandbox_kind != Some(SandboxKind::GitWorktree)
            || session.sandbox_cleanup_state != Some(SandboxCleanupState::Live)
        {
            return false;
        }
        session.sandbox_root.as_deref().is_none_or(|root| {
            !matches!(
                crate::sandbox::git_worktree::observe_tracked_clean_idle(root),
                Ok(true)
            )
        })
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

    /// Legacy callers create a HARD pause; only the operator or an admitted
    /// manager restart can clear an operator marker.
    pub fn record_manager_operator_pause(&self, session: Uuid, paused: bool) -> Result<()> {
        self.set_operator_pause(
            session,
            if paused {
                OperatorPause::Hard
            } else {
                OperatorPause::None
            },
        )?;
        Ok(())
    }

    pub fn set_operator_pause(&self, session: Uuid, pause: OperatorPause) -> Result<OperatorPause> {
        if self.get_session(session)?.is_none() {
            return Err(crate::error::DaemonError::SessionNotFound(session));
        }
        self.conn.execute("INSERT INTO daemon_settings(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",params![format!("manager_operator_pause:{session}"),pause.as_str(),now()])?;
        Ok(pause)
    }

    pub fn get_operator_pause(&self, session: Uuid) -> Result<OperatorPause> {
        if self.get_session(session)?.is_none() {
            return Err(crate::error::DaemonError::SessionNotFound(session));
        }
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM daemon_settings WHERE key=?1",
                [format!("manager_operator_pause:{session}")],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match value.as_deref() {
            None | Some("false") => OperatorPause::None,
            Some("soft") => OperatorPause::Soft,
            _ => OperatorPause::Hard,
        })
    }

    /// #1541: the pause level plus what the operator needs to find a stale
    /// marker: when it was last written and whether a manager succession for
    /// this seat is queued behind it (#1539 holds, never drops, it).
    pub fn get_operator_pause_detail(&self, session: Uuid) -> Result<OperatorPauseDetail> {
        let level = self.get_operator_pause(session)?;
        if level == OperatorPause::None {
            return Ok(OperatorPauseDetail {
                level,
                since: None,
                held_succession: false,
            });
        }
        let since: Option<String> = self
            .conn
            .query_row(
                "SELECT updated_at FROM daemon_settings WHERE key=?1",
                [format!("manager_operator_pause:{session}")],
                |r| r.get(0),
            )
            .optional()?;
        let held_succession = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_root_successions WHERE predecessor_session_id=?1 AND state IN ('reserved','executing'))",
            [session.to_string()],
            |r| r.get(0),
        )?;
        Ok(OperatorPauseDetail {
            level,
            since,
            held_succession,
        })
    }

    pub(crate) fn clear_soft_operator_pause(
        &self,
        session: Uuid,
        actor: Uuid,
        action: &str,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE daemon_settings SET value='false',updated_at=?2 WHERE key=?1 AND value='soft'",
            params![format!("manager_operator_pause:{session}"), now()],
        )?;
        if changed != 0 {
            tracing::info!(%session, %actor, action, "manager cleared soft operator pause");
        }
        Ok(changed != 0)
    }

    pub fn manager_action_operator_paused(&self, target: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM daemon_settings WHERE key=?1 AND value<>'false')",
            [format!("manager_operator_pause:{target}")],
            |r| r.get(0),
        )?)
    }

    pub fn manager_action_human_gate(&self, target: Uuid) -> Result<()> {
        self.manager_action_human_gate_with_interrupted_resume(target, false)
    }

    fn manager_action_archive_gate(&self, target: Uuid) -> Result<()> {
        self.recovery_owner_gate(target, RecoveryOwnerMode::ManagerArchive)
    }

    pub(crate) fn manager_action_descendant_ids(&self, root: Uuid) -> Result<Vec<Uuid>> {
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

    pub fn manager_action_container_tree_ids(&self, root: Uuid) -> Result<Vec<Uuid>> {
        self.manager_action_descendant_ids(root)?
            .into_iter()
            .filter_map(|id| match self.get_session(id) {
                Ok(Some(session)) if session.status != SessionStatus::Deleted => Some(Ok(id)),
                Ok(Some(_)) => None,
                Ok(None) => Some(Err(refused("manager_v2_container_changed"))),
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    fn manager_action_validate_archive_cascade(&self, root: Uuid) -> Result<Vec<(Uuid, bool)>> {
        let ids = self.manager_action_descendant_ids(root)?;
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let session = self
                .get_session(id)?
                .ok_or_else(|| refused("manager_v2_container_changed"))?;
            if session.status == SessionStatus::Deleted {
                continue;
            }
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
                self.manager_action_archive_gate(id)?;
                if Self::manager_action_live_worktree_blocked(&session) {
                    return Err(refused("manager_v2_retention_live_worktree"));
                }
            }
            result.push((id, archived));
        }
        Ok(result)
    }

    /// IDs newly archived by the current successful cascade archive for this
    /// container. A later manager restore consumes the record; an external row
    /// update also invalidates it by advancing `sessions.updated_at`.
    pub fn manager_action_cascade_archive_ids(
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
                if status == "Deleted" {
                    continue;
                }
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
    pub fn manager_action_human_gate_with_interrupted_resume(
        &self,
        target: Uuid,
        allow_interrupted_resume: bool,
    ) -> Result<()> {
        self.recovery_owner_gate(
            target,
            RecoveryOwnerMode::ManagerAction {
                allow_interrupted_resume,
                allow_soft_operator_pause: false,
            },
        )
    }

    /// #1408: gate for a manager `succeed_manager`; see
    /// [`RecoveryOwnerMode::ManagerSuccession`].
    pub(crate) fn manager_succession_human_gate(&self, target: Uuid) -> Result<()> {
        self.recovery_owner_gate(target, RecoveryOwnerMode::ManagerSuccession)
    }

    pub fn manager_restart_human_gate(
        &self,
        target: Uuid,
        allow_interrupted_resume: bool,
    ) -> Result<()> {
        self.recovery_owner_gate(
            target,
            RecoveryOwnerMode::ManagerAction {
                allow_interrupted_resume,
                allow_soft_operator_pause: true,
            },
        )
    }

    /// #953: gate for an explicit manager `replace_lead`/`retry_lead` of the
    /// current lead; see [`RecoveryOwnerMode::ManagerLeadHandover`].
    pub fn manager_lead_handover_human_gate(&self, target: Uuid) -> Result<()> {
        self.recovery_owner_gate(target, RecoveryOwnerMode::ManagerLeadHandover)
    }

    /// #1042: gate for a manager `pause_lead`; see
    /// [`RecoveryOwnerMode::ManagerLeadPause`].
    pub(crate) fn manager_lead_pause_human_gate(&self, target: Uuid) -> Result<()> {
        self.recovery_owner_gate(target, RecoveryOwnerMode::ManagerLeadPause)
    }

    /// The one recovery-owner check shared by manager lifecycle actions and
    /// `AgentArchiveChild`: the held-row SQL plus the program gate.
    ///
    /// Manager archive tolerates operator pause, C5, and resume owners without
    /// clearing the pause marker. Agent archive settles C5 in its own commit.
    /// Other holds and the program gate still apply. Reads through `self.conn` also
    /// observe writes in an open transaction on that connection.
    pub(crate) fn recovery_owner_gate(&self, target: Uuid, mode: RecoveryOwnerMode) -> Result<()> {
        let (
            hold_on_c5_marker,
            hold_on_operator_pause,
            hold_on_resume,
            allow_interrupted_resume,
            allow_soft_operator_pause,
        ) = match mode {
            RecoveryOwnerMode::ManagerAction {
                allow_interrupted_resume,
                allow_soft_operator_pause,
            } => (
                true,
                true,
                true,
                allow_interrupted_resume,
                allow_soft_operator_pause,
            ),
            RecoveryOwnerMode::ManagerArchive => (false, false, false, false, false),
            RecoveryOwnerMode::AgentArchive => (false, true, true, false, false),
            RecoveryOwnerMode::ManagerLeadHandover => (true, true, false, false, true),
            RecoveryOwnerMode::ManagerLeadPause => (true, true, false, false, false),
            RecoveryOwnerMode::ManagerSuccession => (true, false, true, false, false),
        };
        let held: bool = self.conn.query_row(
            "SELECT pending_question_json IS NOT NULL OR pending_archive=1 OR status='WaitingApproval'
             OR EXISTS(SELECT 1 FROM approvals a WHERE session_id=?1 AND status='Pending' AND NOT EXISTS(SELECT 1 FROM appserver_approval_publications p WHERE p.approval_id=a.id AND (p.closure_state='closed' OR p.state='superseded')))
             OR EXISTS(SELECT 1 FROM appserver_approval_publications WHERE session_id=?1 AND closure_state<>'closed' AND state<>'superseded')
             OR EXISTS(SELECT 1 FROM daemon_settings WHERE (?5 AND key=?3) OR (?6 AND key=?2 AND value<>'false' AND (NOT ?7 OR value<>'soft')))
             OR EXISTS(SELECT 1 FROM scheduled_jobs WHERE ?8 AND enabled=1 AND wake_mode='resume' AND wake_session_id=?1 AND id<>?4)
             OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents WHERE state='open' AND controller_session_id=?1)
             OR EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents i JOIN model_invocations m ON m.id=i.last_capacity_model_invocation_id WHERE i.state='open' AND m.session_id=?1)
             FROM sessions WHERE id=?1", params![target.to_string(), format!("manager_operator_pause:{target}"), super::daemon_settings::c5_autofile_pending_key(target), crate::store_support::schedule_wake_job::deterministic_program_guard_job_id(target).to_string(), hold_on_c5_marker, hold_on_operator_pause, allow_soft_operator_pause, hold_on_resume], |r| r.get(0))?;
        if held {
            return Err(refused("manager_v2_human_or_recovery_owner"));
        }
        crate::store_support::manager_gates::check_manager_action_program_gate(
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
        self.manager_launch_policy_gate(&authority.config, &policy.allowed_launches, launch)?;
        let epic = exclude
            .map(|id| self.get_session(id))
            .transpose()?
            .flatten()
            .and_then(|s| s.parent_id)
            .filter(|id| authority.config.epic_ids.contains(id));
        // #1274: the acting principal's own resolved policy (a global seat's
        // granted project policy), then every ancestor's caps.
        self.manager_v2_resource_gate_with(
            &authority.config,
            (!authority.grant.revoked).then_some(&authority.grant.policy),
            epic,
            launch.provider,
            exclude,
            true,
            // #1309: the acting principal originates this launch.
            &[],
        )
    }

    /// Lifetime-within-scope creation usage charged against
    /// `max_created_{containers,sessions}` (#674). Reservations count as well
    /// as committed creations, and policy edits do not reset it. DB-native
    /// review launches are not charged, and a blocked or revoked operation
    /// that never created its target is free.
    pub fn manager_v2_created_usage(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        container: bool,
    ) -> Result<i64> {
        let count: i64 = self.conn.query_row(&format!("SELECT count(*) FROM harness_manager_v2_operations o WHERE o.project_id=?1 AND o.manager_session_id=?2 AND o.scope_version=?3 AND o.kind=?4 AND json_extract(o.payload_json,'$.request.operation.action') IN (SELECT value FROM json_each(?5)) AND NOT EXISTS(SELECT 1 FROM manager_review_assignments r WHERE r.action_operation_id=o.id) AND {CREATION_CHARGED}"), params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,ACTION_KIND,if container { "[\"create_container\"]" } else { "[\"create_session\",\"replace_lead\",\"retry_lead\",\"succeed_manager\"]" }], |r|r.get(0))?;
        if container {
            return Ok(count);
        }
        // Root occurrences retain their creation charge across scope/appointment
        // changes. They are not a recovery and are counted exactly once. A
        // blocked or revoked occurrence never created its candidate and is
        // free, as its operation is (#1390).
        let retained: i64 = self.conn.query_row(
            "SELECT count(*) FROM manager_root_successions r WHERE r.project_id=?1 AND NOT (r.manager_session_id=?2 AND r.scope_version=?3) AND NOT (r.state IN ('blocked','revoked') AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.id=r.candidate_session_id))",
            params![config.project_id.to_string(), config.manager_session_id.to_string(), config.row_version],
            |r| r.get(0),
        )?;
        // #633: sessions launched by topology executions this manager
        // requested in the current scope are charged to the same quota.
        let topology = crate::store_support::topology_usage::topology_created_usage(
            &self.conn,
            config.project_id,
            config.row_version,
        )?;
        Ok(count + retained + topology)
    }

    /// Test fixture: journal a lifecycle operation in an exact state for the
    /// current seat and scope without running admission or execution.
    #[cfg(any(test, feature = "test-seam"))]
    pub fn seed_manager_action_for_test(
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
            sandbox_source: None,
        };
        let request = AgentManagerControlRequestV2 {
            project_id: None,
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
        self.manager_ancestor_creation_budget(authority, container)?;
        if epic && !authority.config.has_dynamic_scope() {
            let pending: i64 = self.conn.query_row("SELECT count(*) FROM harness_manager_v2_operations WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND state IN ('queued','running','uncertain') AND json_extract(payload_json,'$.request.operation.action')='create_container' AND json_extract(payload_json,'$.request.operation.kind')='Epic'", params![authority.config.project_id.to_string(),authority.config.manager_session_id.to_string(),authority.config.row_version,ACTION_KIND], |r|r.get(0))?;
            if authority.config.epic_ids.len() as i64 + pending >= 32 {
                return Err(refused("manager_v2_scope_limit"));
            }
        }
        Ok(())
    }

    /// #1235 rule (d), budgets are charged up the chain: an admission in a
    /// project counts against the project-policy caps of the acting node and
    /// of every ancestor covering the project (#1237: every depth). #1301,
    /// plan §2.3(d): work is charged once, to the originating node and its
    /// ancestors. A node's count is the charged creations in the project
    /// since its authority epoch began whose origin is the node itself, a
    /// node below it, or a principal under the whole chain (the project
    /// manager, an area node, an Epic lead); never an ancestor's or a
    /// sibling's. So a descendant's cap never limits an ancestor's own
    /// admission, and an ancestor's own work never starves a descendant.
    fn manager_ancestor_creation_budget(
        &self,
        authority: &ManagerAuthorityV2,
        container: bool,
    ) -> Result<()> {
        self.manager_ancestor_creation_allowance(&authority.config, container, 1, 0)
    }

    /// [`Self::manager_ancestor_creation_budget`] for `needed` new sessions
    /// (or containers), of which `counted` are already charged (a relaunch of
    /// a charged topology attempt). #1275: a session count includes the
    /// session attempts of manager-requested topology executions in the
    /// project (by the same origin rule), so no session-creating path escapes
    /// an ancestor's allowance.
    pub fn manager_ancestor_creation_allowance(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        container: bool,
        needed: i64,
        counted: i64,
    ) -> Result<()> {
        let (ancestors, own) = self.portfolio_ancestors_of(config)?;
        let charged: Vec<_> = ancestors
            .iter()
            .map(|head| (head, false))
            .chain(own.iter().map(|head| (head, true)))
            .collect();
        let Some(since) = charged.iter().map(|(head, _)| head.started.as_str()).min() else {
            return Ok(());
        };
        let charges = self.portfolio_creation_charges(config.project_id, container, since)?;
        for (head, is_own) in charged {
            let policy = &head.record.grant.project_policy;
            let cap = if container {
                policy.max_created_containers
            } else {
                policy.max_created_sessions
            };
            let count = charges
                .iter()
                .filter(|(created, lineage)| {
                    created.as_str() >= head.started.as_str()
                        && lineage
                            .as_ref()
                            .is_none_or(|lineage| lineage.contains(&head.node_id))
                })
                .count() as i64;
            if (count - counted).saturating_add(needed) > i64::from(cap) {
                return Err(refused(if is_own {
                    "manager_v2_creation_limit"
                } else {
                    rsi_common::global_manager::MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED
                }));
            }
        }
        Ok(())
    }

    /// #1301: every charged creation in `project` since `since`, as its
    /// `created_at` and its origin's lineage (the originating node and the
    /// nodes above it), or `None` when no portfolio node originated it.
    /// Lifecycle operations are attributed by their ledger principal, topology
    /// session attempts and delegated appointments (sessions only) by their
    /// requesting seat and scope, or their grantor node (#1314).
    fn portfolio_creation_charges(
        &self,
        project: Uuid,
        container: bool,
        since: &str,
    ) -> Result<Vec<(String, Option<Vec<Uuid>>)>> {
        let actions = if container {
            "[\"create_container\"]"
        } else {
            "[\"create_session\",\"replace_lead\",\"retry_lead\",\"succeed_manager\"]"
        };
        let mut statement = self.conn.prepare(&format!("SELECT o.created_at,o.manager_session_id,o.scope_version FROM harness_manager_v2_operations o WHERE o.project_id=?1 AND o.kind=?2 AND o.created_at>=?3 AND json_extract(o.payload_json,'$.request.operation.action') IN (SELECT value FROM json_each(?4)) AND NOT EXISTS(SELECT 1 FROM manager_review_assignments r WHERE r.action_operation_id=o.id) AND {CREATION_CHARGED}"))?; // sql-dynamic-ok: static CREATION_CHARGED clause
        let mut rows: Vec<(String, Option<String>, Option<i64>)> = statement
            .query_map(
                params![project.to_string(), ACTION_KIND, since, actions],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !container {
            rows.extend(
                crate::store_support::topology_usage::topology_created_charges_since(
                    &self.conn, project, since,
                )?,
            );
        }
        let mut origins: std::collections::HashMap<(String, i64), Option<Vec<Uuid>>> =
            std::collections::HashMap::new();
        let mut charges = Vec::with_capacity(rows.len());
        for (created, session, version) in rows {
            let lineage = match (session, version) {
                (Some(session), Some(version)) => {
                    let key = (session, version);
                    if let Some(known) = origins.get(&key) {
                        known.clone()
                    } else {
                        let lineage = self
                            .portfolio_origin_node(&key.0, key.1)?
                            .map(|node| self.portfolio_lineage(node))
                            .transpose()?;
                        origins.insert(key, lineage.clone());
                        lineage
                    }
                }
                _ => None,
            };
            charges.push((created, lineage));
        }
        if !container {
            // #1314: a delegated appointment's seat is a session its
            // grantor created, charged to the grantor and its ancestors.
            for (created, node) in self.appointment_creation_charges(project, since)? {
                charges.push((created, Some(self.portfolio_lineage(node)?)));
            }
        }
        Ok(charges)
    }

    /// The authenticated directory a manager action forks `source` from.
    fn manager_action_source_root(&self, source: &Session) -> Result<PathBuf> {
        use crate::sandbox::custody::{CustodyClassification, CustodyService};
        Ok(match CustodyService::classify(source)? {
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
        })
    }

    pub(super) fn manager_action_freeze_source(
        &self,
        source: &Session,
    ) -> Result<ManagerActionSourceV2> {
        let root = self.manager_action_source_root(source)?;
        let (clean, commit) = crate::sandbox::git_worktree::observe_clean_head_bounded(&root)?;
        if !clean {
            return Err(refused("manager_v2_source_worktree_dirty"));
        }
        let branch = crate::sandbox::git_worktree::observe_head_branch_bounded(&root)?;
        Ok(ManagerActionSourceV2 {
            session_id: source.id,
            working_dir: source.working_dir.clone(),
            sandbox_root: source.sandbox_root.clone(),
            commit,
            custody_generation: self.manager_action_custody_generation(source.id)?,
            historical_commit: false,
            branch,
            sandbox_source: None,
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
        if let Some(reclaimed) = self.manager_review_reclaimed_source(source)? {
            return Ok(ManagerActionSourceV2 {
                session_id: source.id,
                working_dir: reclaimed.repository,
                sandbox_root: None,
                commit: expected_commit.to_string(),
                custody_generation: Some(reclaimed.generation as i64),
                historical_commit: true,
                branch: None,
                sandbox_source: None,
            });
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
            branch: None,
            sandbox_source: None,
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
                    let source_session = target.as_ref().ok_or_else(|| {
                        // `replace_lead` forks from the current lead. An Epic
                        // with no lead needs `create_session` + `assign_lead`;
                        // say so instead of a generic source refusal.
                        if matches!(request.operation, ReplaceLead { .. }) {
                            refused("manager_v2_replace_lead_requires_lead_use_create_session")
                        } else {
                            refused("manager_v2_source_unavailable")
                        }
                    })?;
                    match &request.operation {
                        // #1195: a new worker branches from published rolling
                        // (or the named commit/worktree), never from whatever
                        // the shared checkout happens to have checked out.
                        CreateSession { sandbox_source, .. } => self
                            .manager_action_selected_source(
                                source_session,
                                sandbox_source.as_ref(),
                            )?,
                        _ => self.manager_action_freeze_source(source_session)?,
                    }
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
                    self.manager_resume_available_for_lead(lead)?;
                }
                if let RetryLead { launch: choice, .. } = &request.operation {
                    self.manager_retry_admissible_for_lead(lead)?;
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

    pub fn enqueue_manager_action(
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
        self.enqueue_manager_action_with_source_on(origin, request, None, None)
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
        self.enqueue_manager_action_with_source_on(origin, request, Some(&source), None)
    }

    fn enqueue_manager_action_with_source_on(
        &self,
        origin: ManagerActionOriginV2,
        request: AgentManagerControlRequestV2,
        review_source: Option<&ManagerActionSourceV2>,
        issue_binding: Option<&ManagerIssueBindingV1>,
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
        let mut payload = json!({"origin":origin,"request":request});
        if let Some(binding) = issue_binding {
            payload["issue_binding"] = serde_json::to_value(binding)?;
        }
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
        if matches!(
            &request.operation,
            ResumeLead { .. } | RetryLead { .. } | ReplaceLead { .. }
        ) {
            let epic = action_epic(&request.operation).expect("lead action");
            self.manager_v2_inherit_lead_pause(&authority.config, epic)?;
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
            sandbox_source: admission
                .source
                .as_ref()
                .and_then(sandbox_source::sandbox_source_receipt),
        };
        let stamp = now();
        let due = (Utc::now() + chrono::Duration::seconds(i64::from(admission.delay_seconds)))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let actor = match origin {
            ManagerActionOriginV2::Agent { caller } => Some(caller),
            ManagerActionOriginV2::OperatingIntent { .. } => None,
        };
        self.conn.execute("INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,scope_version,policy_version,actor_session_id,idempotency_key,fingerprint,kind,payload_json,state,row_version,target_session_id,outcome_json,not_before,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'queued',1,?11,?12,?13,?14,?14)", params![operation_id.to_string(),authority.config.project_id.to_string(),authority.config.manager_session_id.to_string(),authority.config.row_version,authority.grant.row_version,actor.map(|v|v.to_string()),request.idempotency_key,fingerprint(&payload)?,ACTION_KIND,serde_json::to_string(&payload)?,id.map(|v|v.to_string()),serde_json::to_string(&receipt)?,due,stamp])?;
        // Publish stop intent at admission, before waiting on an earlier
        // action or a process guard. Retirement must close the gap before its
        // wake-disabling effect; automatic recovery re-reads this pause.
        let pause = match &request.operation {
            PauseLead {
                epic_id,
                expected,
                reason,
            } => Some((*epic_id, expected.lead_session_id, reason.as_str())),
            RetireLeadContinuations { epic_id, expected } => {
                Some((*epic_id, expected.lead_session_id, RETIREMENT_PAUSE_REASON))
            }
            _ => None,
        };
        if let Some((epic_id, lead_session_id, reason)) = pause {
            self.manager_v2_set_lead_pause(
                &authority.config,
                epic_id,
                operation_id,
                actor,
                lead_session_id,
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
            issue_binding: issue_binding.cloned(),
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

    /// #1084: launch targets of terminal manager actions whose failed-launch
    /// cleanup is still owed: the row is still Starting, or already Failed with
    /// its model invocation still open. Read-only; the settlement itself runs
    /// under the session manager's fences.
    pub fn manager_launch_cleanup_candidates(&self, limit: i64) -> Result<Vec<Uuid>> {
        // #1143: an action carrying drain-pending cleanup debt is excluded
        // here: its owed process/task exit must be proved before any row or
        // invocation settles (`manager_launch_drain_pending_targets`).
        let mut stmt = self.conn.prepare(
            "SELECT o.target_session_id
             FROM harness_manager_v2_operations o
             JOIN sessions s ON s.id = o.target_session_id
             LEFT JOIN model_invocations mi ON mi.id = s.model_invocation_id
             WHERE o.kind = ?1
               AND o.state IN ('failed','blocked','revoked','uncertain')
               AND COALESCE(json_extract(o.outcome_json,'$.outcome'),'') <> ?3
               AND json_extract(o.payload_json,'$.request.operation.action')
                   IN ('create_session','replace_lead','retry_lead')
               AND (s.status = 'Starting'
                    OR (s.status = 'Failed'
                        AND mi.status IN ('running','cancellation_requested')))
             ORDER BY o.updated_at
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![ACTION_KIND, limit, MANAGER_LAUNCH_DRAIN_PENDING],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| parse_id(&row?)).collect()
    }

    /// #1143: candidates of `uncertain` actions that still owe a verified
    /// drain (the launch deadline could not prove the provider process and
    /// its task gone). The debt lasts while the row is `Starting` or its
    /// invocation is open; settling both ends it.
    pub fn manager_launch_drain_pending_targets(&self, limit: i64) -> Result<Vec<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT o.target_session_id
             FROM harness_manager_v2_operations o
             JOIN sessions s ON s.id = o.target_session_id
             LEFT JOIN model_invocations mi ON mi.id = s.model_invocation_id
             WHERE o.kind = ?1
               AND o.state = 'uncertain'
               AND json_extract(o.outcome_json,'$.outcome') = ?3
               AND json_extract(o.payload_json,'$.request.operation.action')
                   IN ('create_session','replace_lead','retry_lead')
               AND (s.status = 'Starting'
                    OR mi.status IN ('running','cancellation_requested'))
             ORDER BY o.updated_at
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![ACTION_KIND, limit, MANAGER_LAUNCH_DRAIN_PENDING],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| parse_id(&row?)).collect()
    }

    /// #1143: record that the owed drain was proved and settled: the receipt
    /// keeps its `uncertain` state but names the timeout class instead of the
    /// debt, so the sweep stops revisiting it.
    pub fn clear_manager_launch_drain_pending(&self, target: Uuid) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE harness_manager_v2_operations
             SET outcome_json = json_set(outcome_json,'$.outcome',?4), updated_at = ?5
             WHERE kind = ?1 AND target_session_id = ?2 AND state = 'uncertain'
               AND json_extract(outcome_json,'$.outcome') = ?3",
            params![
                ACTION_KIND,
                target.to_string(),
                MANAGER_LAUNCH_DRAIN_PENDING,
                MANAGER_LAUNCH_DRAIN_VERIFIED,
                now()
            ],
        )?)
    }

    pub fn manager_action_operation(&self, id: Uuid) -> Result<Option<ManagerActionOperationV2>> {
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

    /// Queued `create_session` actions that are due, oldest first (the order
    /// the claim query releases them in): `(target session id, not_before)`.
    /// Read-only; `AgentGetDaemonInfo` host-load admission lists them (#1417).
    pub fn queued_manager_create_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<(Option<Uuid>, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT o.target_session_id,o.not_before FROM harness_manager_v2_operations o
             WHERE o.kind=?1 AND o.state='queued' AND o.not_before<=?3
               AND json_extract(o.payload_json,'$.request.operation.action')='create_session'
               AND json_extract(o.payload_json,'$.issue_binding.continue_from') IS NULL
             ORDER BY o.not_before,o.id LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![ACTION_KIND, i64::try_from(limit).unwrap_or(i64::MAX), now()],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
        )?;
        let mut queued = Vec::new();
        for row in rows {
            let (target, not_before) = row?;
            queued.push((target.map(|id| parse_id(&id)).transpose()?, not_before));
        }
        Ok(queued)
    }

    pub fn claim_manager_action(&self, boot_id: Uuid) -> Result<Option<ManagerActionClaimV2>> {
        self.claim_manager_action_holding(boot_id, false)
    }

    /// `hold_worker_starts` (#1073 deploy drain) leaves every action that
    /// would start a worker turn queued and unclaimed, so a restart re-admits
    /// it instead of finding a `running` claim it must mark uncertain.
    pub fn claim_manager_action_holding(
        &self,
        boot_id: Uuid,
        hold_worker_starts: bool,
    ) -> Result<Option<ManagerActionClaimV2>> {
        self.claim_manager_action_holding_creates(boot_id, hold_worker_starts, false)
    }

    /// [`Self::claim_manager_action_holding`] plus `hold_creates` (#1417 host
    /// load): leaves new queued `create_session` actions (Issue-worker launches and
    /// reviews included) unclaimed while the host is too loaded for a new
    /// worker. Lead recovery (`replace_lead`, `retry_lead`, `resume_lead`)
    /// Issue worker continuations, and every other action still claim, so only
    /// new work waits. The held
    /// creates stay queued, so they keep their `(not_before, id)` place and
    /// the claim query releases them oldest-first across projects.
    pub fn claim_manager_action_holding_creates(
        &self,
        boot_id: Uuid,
        hold_worker_starts: bool,
        hold_creates: bool,
    ) -> Result<Option<ManagerActionClaimV2>> {
        self.claim_manager_action_with_admission(boot_id, hold_worker_starts, hold_creates, None)
    }

    /// Consult admission only for the oldest eligible new create. The callback
    /// must be synchronous and must not access the store. A held create stays
    /// queued; other actions, including continuations, can still claim.
    pub fn claim_manager_action_with_create_admission(
        &self,
        boot_id: Uuid,
        hold_worker_starts: bool,
        admit: &dyn Fn(Uuid, chrono::DateTime<chrono::Utc>, Option<Uuid>) -> bool,
    ) -> Result<Option<ManagerActionClaimV2>> {
        self.claim_manager_action_with_admission(boot_id, hold_worker_starts, false, Some(admit))
    }

    fn claim_manager_action_with_admission(
        &self,
        boot_id: Uuid,
        hold_worker_starts: bool,
        hold_creates: bool,
        admit: Option<&dyn Fn(Uuid, chrono::DateTime<chrono::Utc>, Option<Uuid>) -> bool>,
    ) -> Result<Option<ManagerActionClaimV2>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // One active command per project, even across independent daemon handles.
        // An uncertain command fences only conflicting targets, not all future work.
        // Preserve the oldest eligible claim except when that claim is
        // automatic lead recovery and a due agent-created child in the same
        // project is ready. Review allocations use that same CreateSession
        // journal; the original conflict and due predicates apply to both.
        // #1143: running creates overlap only across distinct parents. A
        // same-parent (or unreadable-parent: `IS NOT` fails closed) running
        // create fences the candidate; the runtime keeps a matching
        // per-parent guard (`manager_create_parent_guard_key`).
        const CREATE_PAIR: &str =
            "json_extract(o.payload_json,'$.request.operation.action')='create_session'";
        let selected:Option<(String,String)>=self.conn.query_row(
            &format!("WITH eligible AS (
             SELECT o.id,o.project_id,o.not_before,
                    json_extract(o.payload_json,'$.origin.origin') AS origin,
                    json_extract(o.payload_json,'$.request.operation.action') AS action
               FROM harness_manager_v2_operations o WHERE o.kind=?1 AND o.state='queued' AND o.not_before<=?2
             AND (?3=0 OR json_extract(o.payload_json,'$.request.operation.action') NOT IN ('create_session','replace_lead','retry_lead','resume_lead'))
             AND (?5=0 OR json_extract(o.payload_json,'$.request.operation.action')<>'create_session'
                  OR json_extract(o.payload_json,'$.issue_binding.continue_from') IS NOT NULL)
             AND NOT EXISTS(SELECT 1 FROM harness_manager_v2_operations a WHERE a.project_id=o.project_id AND a.kind=?1
                AND a.id IS NOT json_extract(o.payload_json,'$.request.operation.operation_id') AND ((a.state='running' AND NOT ({CREATE_PAIR} AND json_extract(a.payload_json,'$.request.operation.action')='create_session' AND json_extract(a.payload_json,'$.request.operation.parent_id') IS NOT json_extract(o.payload_json,'$.request.operation.parent_id'))) OR (a.state='uncertain' AND (a.target_session_id=o.target_session_id OR (json_extract(a.payload_json,'$.request.operation.epic_id')=json_extract(o.payload_json,'$.request.operation.epic_id') AND {UNCERTAINTY_CURRENT_AUTHORITY})))))
             AND (json_extract(o.payload_json,'$.request.operation.action')<>'create_session' OR (
                  (SELECT COUNT(*) FROM harness_manager_v2_operations a WHERE a.project_id=o.project_id AND a.kind=?1 AND a.state='running')<?4
              AND (NOT EXISTS(SELECT 1 FROM harness_manager_v2_operations a WHERE a.project_id=o.project_id AND a.kind=?1 AND a.state='running')
                   OR NOT EXISTS(SELECT 1 FROM harness_manager_v2_operations q WHERE q.project_id=o.project_id AND q.kind=?1 AND q.state='queued'
                        AND json_extract(q.payload_json,'$.request.operation.action')<>'create_session' AND (q.not_before,q.id)<(o.not_before,o.id)))))
             AND NOT EXISTS(SELECT 1 FROM manager_root_successions r JOIN sessions p ON p.id=r.predecessor_session_id
                WHERE r.operation_id=o.id AND r.state='reserved'
                  AND (p.status IN ('Starting','Running','WaitingApproval')
                       OR EXISTS(SELECT 1 FROM daemon_settings d WHERE d.key='manager_operator_pause:'||p.id AND d.value<>'false'))
                  AND r.authority_epoch=(SELECT epoch FROM manager_authority_epochs WHERE project_id=r.project_id)
                  AND EXISTS(SELECT 1 FROM harness_manager_scopes h WHERE h.project_id=r.project_id AND h.row_version=r.scope_version))
             ), oldest AS (SELECT * FROM eligible ORDER BY not_before,id LIMIT 1)
             SELECT e.id,e.not_before FROM eligible e CROSS JOIN oldest first
              WHERE e.id=first.id OR (
                    first.origin='operating_intent'
                AND first.action IN ('resume_lead','retry_lead','replace_lead')
                AND e.project_id=first.project_id
                AND e.origin='agent' AND e.action='create_session')
              ORDER BY CASE WHEN e.id=first.id THEN 1 ELSE 0 END,e.not_before,e.id LIMIT 1"),params![ACTION_KIND,now(),i64::from(hold_worker_starts),MANAGER_CREATE_PARALLELISM,i64::from(hold_creates)],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((id, not_before)) = selected else {
            return Ok(None);
        };
        let id = parse_id(&id)?;
        let mut operation = self
            .manager_action_operation(id)?
            .ok_or_else(|| refused("manager_v2_action_unavailable"))?;
        if operation.is_new_worker_create()
            && let Some(admit) = admit
        {
            let since = chrono::DateTime::parse_from_rfc3339(&not_before)
                .map_err(|_| refused("manager_v2_action_invalid_not_before"))?
                .with_timezone(&chrono::Utc);
            if !admit(id, since, operation.receipt.target_session_id) {
                drop(tx);
                return self.claim_manager_action_holding_creates(
                    boot_id,
                    hold_worker_starts,
                    true,
                );
            }
        }
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

    /// #1073: hand a claimed action back to the queue when the deploy drain
    /// engaged after the claim and before any provider effect. Returns `false`
    /// (leaving the claim untouched) once an effect started: that outcome is
    /// no longer safe to replay.
    pub fn requeue_held_manager_action(&self, claim: &ManagerActionClaimV2) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let op = self.manager_action_assert_claim(claim)?;
        if op.effect_started {
            return Ok(false);
        }
        let mut receipt = op.receipt;
        receipt.state = ManagerActionStateV2::Queued;
        receipt.outcome = None;
        receipt.row_version += 1;
        receipt.refresh_action_metadata(&op.context.request.operation);
        let changed = self.conn.execute(
            "UPDATE harness_manager_v2_operations SET state='queued',row_version=?2,outcome_json=?3,attempts=MAX(attempts-1,0),claim_boot_id=NULL,updated_at=?4 WHERE id=?1 AND state='running' AND row_version=?5 AND claim_boot_id=?6",
            params![
                claim.id().to_string(),
                receipt.row_version,
                serde_json::to_string(&receipt)?,
                now(),
                claim.operation.receipt.row_version,
                claim.boot_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(refused("manager_v2_claim_changed"));
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn recover_manager_actions_startup(&self, boot_id: Uuid) -> Result<usize> {
        self.recover_manager_action_claims(Some(boot_id))
    }

    /// Caller must own the runtime reconciliation single-flight guard. No
    /// previous execution future can then own a running claim, including an
    /// aborted future from this same boot. Never retry its provider effect.
    pub fn recover_abandoned_manager_action_claims(&self) -> Result<usize> {
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
    pub fn manager_action_runtime_gate(
        &self,
        claim: &ManagerActionClaimV2,
        effect: bool,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        if effect {
            let op = self.manager_action_assert_claim(claim)?;
            let authority =
                self.manager_stored_action_authority(&op.context.origin, &op.context.request)?;
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
        // #967: a DB review launch whose assignment already ended must not
        // start (or continue toward) a reviewer provider turn.
        self.require_manager_review_launch_live(&op)?;
        let authority =
            self.manager_stored_action_authority(&op.context.origin, &op.context.request)?;
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
            self.manager_retry_admissible_for_lead(lead)?;
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
                let (version, paused) = self.manager_v2_action_pause(&authority.config, epic)?;
                // Retirement stops automatic intent recovery at admission,
                // including an intent action queued before the retirement.
                if matches!(
                    op.context.origin,
                    ManagerActionOriginV2::OperatingIntent { .. }
                ) && self.manager_v2_lead_pause(&authority.config, epic)?.1
                {
                    return Err(refused("manager_v2_manager_paused"));
                }
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
                    && !delegated_cleanup_allowed_during_lead_pause(&action)
                    && !matches!(
                        action,
                        ManagerActionV2::AssignLead { .. }
                            | ManagerActionV2::RetireLeadContinuations { .. }
                            | ManagerActionV2::ArchiveContainer { .. }
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
                } else if matches!(action, ManagerActionV2::ArchiveSession { .. })
                    || operator_delegation::delegated_archive_effect(&action)
                {
                    self.manager_action_archive_gate(target.id)?;
                } else {
                    if matches!(op.context.origin, ManagerActionOriginV2::Agent { .. })
                        && matches!(
                            action,
                            ManagerActionV2::RetryLead { .. } | ManagerActionV2::ReplaceLead { .. }
                        )
                    {
                        self.manager_lead_handover_human_gate(target.id)?;
                    } else if matches!(op.context.origin, ManagerActionOriginV2::Agent { .. })
                        && matches!(
                            action,
                            ManagerActionV2::ResumeLead { .. }
                                | ManagerActionV2::AssignLead {
                                    session_id: Some(_),
                                    ..
                                }
                        )
                    {
                        self.manager_restart_human_gate(target.id, allow_interrupted_resume)?;
                    } else {
                        self.manager_action_human_gate_with_interrupted_resume(
                            target.id,
                            allow_interrupted_resume,
                        )?;
                    }
                }
            }
        } else if let Some(target) = target.as_ref() {
            // Pausing may stop a running turn, but never answers or cancels a
            // pending approval/question or takes an independent recovery owner.
            self.manager_lead_pause_human_gate(target.id)?;
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
    pub fn manager_action_accept_settled_fence(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let op = self.manager_action_assert_claim(claim)?;
        let authority =
            self.manager_stored_action_authority(&op.context.origin, &op.context.request)?;
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
    pub fn bind_manager_resume_invocation(
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

    pub fn finish_manager_action(
        &self,
        claim: &ManagerActionClaimV2,
        state: ManagerActionStateV2,
        outcome: &str,
    ) -> Result<ManagerActionReceiptV2> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let receipt = self.finish_manager_action_on(claim, state, outcome)?;
        tx.commit()?;
        // #1553: a launch that ends without its worker tells its Issue. Best
        // effort: the receipt is already durable and is the record.
        if let Err(error) =
            self.note_issue_worker_launch_ended(&claim.operation, receipt.state, outcome)
        {
            tracing::warn!(operation_id = %claim.id(), %error, "issue worker launch note not appended");
        }
        Ok(receipt)
    }

    /// #1042: a succeeded `pause_lead` suspends the fenced lead's resume
    /// wakes and a succeeded `resume_lead` restores exactly those. Shared by the
    /// normal finish and the settlement of an uncertain pause.
    pub(super) fn apply_lead_wake_effect(
        &self,
        action: &ManagerActionV2,
        action_id: Uuid,
    ) -> Result<()> {
        let lead = action_fence(action).and_then(|f| f.lead_session_id);
        match (action, lead) {
            (ManagerActionV2::PauseLead { .. }, Some(lead)) => {
                self.suspend_lead_resume_wakes(lead, action_id)?;
            }
            (ManagerActionV2::ResumeLead { .. }, Some(lead)) => {
                self.restore_lead_suspended_wakes(lead)?;
            }
            _ => {}
        }
        Ok(())
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
        // #1042: the pause's success and the suspension of the lead's resume
        // wakes commit together; `resume_lead` restores exactly that record.
        if state == ManagerActionStateV2::Succeeded {
            self.apply_lead_wake_effect(&op.context.request.operation, claim.id())?;
        }
        if state == ManagerActionStateV2::Succeeded
            && matches!(op.context.origin, ManagerActionOriginV2::Agent { .. })
            && matches!(
                op.context.request.operation,
                ManagerActionV2::ResumeLead { .. }
                    | ManagerActionV2::RetryLead { .. }
                    | ManagerActionV2::ReplaceLead { .. }
                    | ManagerActionV2::AssignLead {
                        session_id: Some(_),
                        ..
                    }
            )
        {
            let actor = match op.context.origin {
                ManagerActionOriginV2::Agent { caller } => caller,
                _ => unreachable!(),
            };
            let mut cleared_targets = action_fence(&op.context.request.operation)
                .and_then(|f| f.lead_session_id)
                .into_iter()
                .collect::<Vec<_>>();
            if let ManagerActionV2::AssignLead {
                session_id: Some(target),
                ..
            } = &op.context.request.operation
            {
                if !cleared_targets.contains(target) {
                    cleared_targets.push(*target);
                }
            }
            for old in cleared_targets {
                if self.clear_soft_operator_pause(old, actor, "manager_restart")? {
                    if let Some(config) = self.get_harness_manager(op.project_id)? {
                        self.manager_v2_event(&config, Some(actor), "operator_pause_cleared", &old.to_string(), receipt.row_version, &json!({"action_id":claim.id(),"action":op.context.request.operation.action_kind(),"level":"soft","session_id":old}))?;
                    }
                }
            }
            if !matches!(
                op.context.request.operation,
                ManagerActionV2::AssignLead { .. }
            ) {
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
    pub fn apply_manager_container_action(
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
    pub fn apply_manager_session_action(&self, claim: &ManagerActionClaimV2) -> Result<Session> {
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
                    "UPDATE sessions SET status='Archived',pending_archive=0,
                     retry_attempt=COALESCE(max_retries,retry_attempt),updated_at=?2
                     WHERE id=?1 AND status IN ('Completed','Failed','Interrupted')",
                    params![id.to_string(), stamp],
                )?;
                if changed != 1 {
                    return Err(refused("manager_v2_session_state_changed"));
                }
                Self::resolve_c5_autofile_pending_tx(&tx, id)?;
                Self::cancel_queued_recovery_for_archive_on(&tx, id)?;
                "session_archived"
            }
            RestoreSession { .. } => {
                let changed = self.conn.execute(
                    "UPDATE sessions SET status='Completed',pending_archive=0,
                     stop_reason=COALESCE(NULLIF(TRIM(stop_reason),''),'completed:restored'),updated_at=?2
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
                    "UPDATE sessions SET status='Archived',lead_session_id=NULL,
                     retry_attempt=COALESCE(max_retries,retry_attempt),updated_at=?2
                     WHERE id=?1 AND status IN ('Completed','Failed','Interrupted')",
                    params![member.to_string(), stamp],
                )?;
                self.conn.execute(
                    "DELETE FROM daemon_settings WHERE key=?1",
                    [super::daemon_settings::c5_autofile_pending_key(*member)],
                )?;
                Self::cancel_queued_recovery_for_archive_on(&self.conn, *member)?;
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
                    "UPDATE sessions SET status='Completed',pending_archive=0,lead_session_id=NULL,
                     stop_reason=COALESCE(NULLIF(TRIM(stop_reason),''),'completed:restored'),updated_at=?2 WHERE id=?1 AND status=?3",
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
    pub fn commit_manager_lead_action(&self, claim: &ManagerActionClaimV2) -> Result<Vec<Uuid>> {
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
            // #953: the superseded lineage never keeps a live resume wake,
            // atomically with the lead CAS above.
            if let Some(old) = expected.lead_session_id.filter(|old| Some(*old) != target) {
                self.retire_handover_lineage_resume_wakes_on(claim, old)?;
            }
        }
        self.finish_manager_action_on(claim, ManagerActionStateV2::Succeeded, "lead_committed")?;
        tx.commit()?;
        Ok(jobs)
    }

    pub(crate) fn commit_manager_created_session(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<()> {
        self.commit_manager_created_session_with_watch(claim, None)
    }

    /// #1115: an Issue-bound launch persists its caller's terminal watch in the
    /// same transaction as the success, so no crash window leaves a launched
    /// worker unwatched. An enabled watch for the same (owner, worker) is kept.
    pub fn commit_manager_created_session_with_watch(
        &self,
        claim: &ManagerActionClaimV2,
        watch: Option<&rsi_common::types::ScheduledJob>,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.manager_action_runtime_gate_on(claim)?;
        self.manager_action_bind_entity(claim, "session")?;
        self.finish_manager_action_on(
            claim,
            ManagerActionStateV2::Succeeded,
            "session_established",
        )?;
        if let Some(watch) = watch {
            self.insert_on_terminal_watch_if_absent_conn(&tx, watch)?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn parse_id(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| refused("manager_v2_invalid_stored_identity"))
}
